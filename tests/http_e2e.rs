//! End-to-end HTTP tests with a real loopback upstream.
//!
//! These bind ephemeral loopback ports. Environments that forbid local
//! binding (some sandboxes) skip them with a clear message; CI runs them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use llm_gateway::config::{Entry, Model, RewriteRule, Target};
use llm_gateway::conversationlog::Store;
use llm_gateway::keymap::Map;
use llm_gateway::logging::JsonLogger;
use llm_gateway::proxy::forward::Forwarder;
use llm_gateway::server::{self, AppState};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn loopback_available() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

#[derive(Clone, Debug)]
struct Recorded {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

struct Upstream {
    address: SocketAddr,
    records: Arc<Mutex<Vec<Recorded>>>,
    shutdown: watch::Sender<bool>,
}

impl Upstream {
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn records(&self) -> Vec<Recorded> {
        self.records.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn start_echo_upstream() -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            async move {
                                let (parts, body) = request.into_parts();
                                let body = body.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body: body.clone(),
                                });
                                let response = Response::builder()
                                    .status(StatusCode::ACCEPTED)
                                    .header("X-Upstream", "yes")
                                    .body(
                                        Full::new(body)
                                            .map_err(|never: Infallible| match never {})
                                            .boxed_unsync(),
                                    )
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    // The task is detached; dropping `shutdown` stops the accept loop.
    drop(task);
    Upstream {
        address,
        records,
        shutdown,
    }
}

async fn start_sse_upstream(
    first_sent: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    let first_sent = Arc::new(Mutex::new(Some(first_sent)));
    let release = Arc::new(Mutex::new(Some(release)));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    let first_sent = first_sent.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            let first_sent = first_sent.lock().unwrap().take();
                            let release = release.lock().unwrap().take();
                            async move {
                                let (parts, body) = request.into_parts();
                                let body = body.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body,
                                });
                                let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(4);
                                tokio::spawn(async move {
                                    let _ = sender.send(Bytes::from_static(b"data: first\n\n")).await;
                                    if let Some(first_sent) = first_sent {
                                        let _ = first_sent.send(());
                                    }
                                    if let Some(release) = release {
                                        let _ = release.await;
                                    }
                                    let _ = sender.send(Bytes::from_static(b"data: last\n\n")).await;
                                });
                                let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                                    receiver.recv().await.map(|chunk| {
                                        (Ok::<_, BoxError>(Frame::data(chunk)), receiver)
                                    })
                                });
                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header(CONTENT_TYPE, "text/event-stream")
                                    .body(StreamBody::new(stream).boxed_unsync())
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    Upstream {
        address,
        records,
        shutdown,
    }
}

/// An upstream that always answers with the same status, content type and body.
async fn start_static_upstream(
    status: StatusCode,
    content_type: &'static str,
    body: &'static [u8],
) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            async move {
                                let (parts, incoming) = request.into_parts();
                                let received = incoming.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body: received,
                                });
                                let response = Response::builder()
                                    .status(status)
                                    .header(CONTENT_TYPE, content_type)
                                    .body(
                                        Full::new(Bytes::from_static(body))
                                            .map_err(|never: Infallible| match never {})
                                            .boxed_unsync(),
                                    )
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    Upstream {
        address,
        records,
        shutdown,
    }
}

/// How the coding test double decides whether to compress its reply.
#[derive(Clone, Copy)]
enum CodingMode {
    /// Compress with the first supported coding the request advertises.
    Negotiated,
    /// Compress with this coding regardless of what was advertised.
    Force(&'static str),
    /// Name this coding in the response without compressing the body.
    Declare(&'static str),
    /// gzip-compress, then cut the stream short.
    TruncatedGzip,
}

async fn encode_body(coding: &str, plain: &[u8]) -> Vec<u8> {
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    use tokio::io::AsyncWriteExt;

    let mut out = Vec::new();
    match coding {
        "gzip" => {
            let mut encoder = GzipEncoder::new(&mut out);
            encoder.write_all(plain).await.unwrap();
            encoder.shutdown().await.unwrap();
        }
        "deflate" => {
            let mut encoder = ZlibEncoder::new(&mut out);
            encoder.write_all(plain).await.unwrap();
            encoder.shutdown().await.unwrap();
        }
        "br" => {
            let mut encoder = BrotliEncoder::new(&mut out);
            encoder.write_all(plain).await.unwrap();
            encoder.shutdown().await.unwrap();
        }
        "zstd" => {
            let mut encoder = ZstdEncoder::new(&mut out);
            encoder.write_all(plain).await.unwrap();
            encoder.shutdown().await.unwrap();
        }
        other => panic!("unknown coding {other}"),
    }
    out
}

fn negotiated_coding(headers: &HeaderMap) -> Option<&'static str> {
    let acceptable: Vec<String> = headers
        .get_all("accept-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .collect();
    ["gzip", "deflate", "br", "zstd"]
        .into_iter()
        .find(|coding| acceptable.iter().any(|token| token == coding))
}

/// An upstream that compresses according to `mode`, so the gateway's
/// negotiation and decoding can be observed from the client side.
async fn start_coding_upstream(
    mode: CodingMode,
    status: StatusCode,
    body: &'static [u8],
) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            async move {
                                let (parts, incoming) = request.into_parts();
                                let received = incoming.collect().await.unwrap().to_bytes();
                                let declared = match mode {
                                    CodingMode::Negotiated => negotiated_coding(&parts.headers),
                                    CodingMode::Force(coding) | CodingMode::Declare(coding) => {
                                        Some(coding)
                                    }
                                    CodingMode::TruncatedGzip => Some("gzip"),
                                };
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body: received,
                                });
                                let bytes = match (mode, declared) {
                                    (CodingMode::Force(coding), _) => {
                                        encode_body(coding, body).await
                                    }
                                    (CodingMode::TruncatedGzip, _) => {
                                        let mut encoded = encode_body("gzip", body).await;
                                        let keep = encoded.len() * 2 / 3;
                                        encoded.truncate(keep.max(1));
                                        encoded
                                    }
                                    (CodingMode::Negotiated, Some(coding)) => {
                                        encode_body(coding, body).await
                                    }
                                    _ => body.to_vec(),
                                };
                                let mut builder = Response::builder()
                                    .status(status)
                                    .header(CONTENT_TYPE, "application/json")
                                    .header("content-length", bytes.len().to_string());
                                if let Some(coding) = declared {
                                    builder = builder.header("content-encoding", coding);
                                }
                                let response = builder
                                    .body(
                                        Full::new(Bytes::from(bytes))
                                            .map_err(|never: Infallible| match never {})
                                            .boxed_unsync(),
                                    )
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    Upstream {
        address,
        records,
        shutdown,
    }
}

/// Writes compressed bytes into a channel the response body reads from.
struct ChannelWriter {
    sender: tokio::sync::mpsc::UnboundedSender<Bytes>,
}

impl tokio::io::AsyncWrite for ChannelWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let count = buf.len();
        match self.sender.send(Bytes::copy_from_slice(buf)) {
            Ok(()) => std::task::Poll::Ready(Ok(count)),
            Err(_) => std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A gzip-encoded SSE upstream that flushes the first event before producing
/// the rest, so incremental decoding can be proven.
async fn start_gzip_sse_upstream(
    first_sent: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> Upstream {
    use async_compression::tokio::write::GzipEncoder;
    use tokio::io::AsyncWriteExt;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    let first_sent = Arc::new(Mutex::new(Some(first_sent)));
    let release = Arc::new(Mutex::new(Some(release)));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    let first_sent = first_sent.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            let first_sent = first_sent.lock().unwrap().take();
                            let release = release.lock().unwrap().take();
                            async move {
                                let (parts, incoming) = request.into_parts();
                                let body = incoming.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body,
                                });
                                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
                                tokio::spawn(async move {
                                    let mut encoder = GzipEncoder::new(ChannelWriter { sender });
                                    encoder.write_all(b"data: first\n\n").await.unwrap();
                                    encoder.flush().await.unwrap();
                                    if let Some(first_sent) = first_sent {
                                        let _ = first_sent.send(());
                                    }
                                    if let Some(release) = release {
                                        let _ = release.await;
                                    }
                                    encoder.write_all(b"data: last\n\n").await.unwrap();
                                    encoder.shutdown().await.unwrap();
                                });
                                let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                                    receiver.recv().await.map(|chunk| {
                                        (Ok::<_, BoxError>(Frame::data(chunk)), receiver)
                                    })
                                });
                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header(CONTENT_TYPE, "text/event-stream")
                                    .header("content-encoding", "gzip")
                                    .body(StreamBody::new(stream).boxed_unsync())
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    Upstream {
        address,
        records,
        shutdown,
    }
}

// One realistic Anthropic Messages SSE turn. The content_block_delta event is
// deliberately split across two writes to prove the gateway forwards a frame
// without reassembling or buffering it.
const ANTHROPIC_MESSAGE_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-real\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
const ANTHROPIC_BLOCK_START: &[u8] = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const ANTHROPIC_DELTA_HEAD: &[u8] =
    b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,";
const ANTHROPIC_DELTA_TAIL: &[u8] = b"\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n";
const ANTHROPIC_BLOCK_STOP: &[u8] =
    b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
const ANTHROPIC_MESSAGE_DELTA: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n";
const ANTHROPIC_MESSAGE_STOP: &[u8] = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// Emits a full Anthropic turn, holding every event after `message_start` until
/// the test releases it, so a client can prove the first event was not buffered.
async fn start_anthropic_sse_upstream(
    first_sent: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let service_records = records.clone();
    let first_sent = Arc::new(Mutex::new(Some(first_sent)));
    let release = Arc::new(Mutex::new(Some(release)));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let records = service_records.clone();
                    let first_sent = first_sent.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request: Request<Incoming>| {
                            let records = records.clone();
                            let first_sent = first_sent.lock().unwrap().take();
                            let release = release.lock().unwrap().take();
                            async move {
                                let (parts, body) = request.into_parts();
                                let body = body.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    method: parts.method,
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body,
                                });
                                let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(8);
                                tokio::spawn(async move {
                                    let _ = sender.send(Bytes::from_static(ANTHROPIC_MESSAGE_START)).await;
                                    if let Some(first_sent) = first_sent {
                                        let _ = first_sent.send(());
                                    }
                                    if let Some(release) = release {
                                        let _ = release.await;
                                    }
                                    for chunk in [
                                        ANTHROPIC_BLOCK_START,
                                        ANTHROPIC_DELTA_HEAD,
                                        ANTHROPIC_DELTA_TAIL,
                                        ANTHROPIC_BLOCK_STOP,
                                        ANTHROPIC_MESSAGE_DELTA,
                                        ANTHROPIC_MESSAGE_STOP,
                                    ] {
                                        let _ = sender.send(Bytes::from_static(chunk)).await;
                                    }
                                });
                                let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                                    receiver.recv().await.map(|chunk| {
                                        (Ok::<_, BoxError>(Frame::data(chunk)), receiver)
                                    })
                                });
                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header(CONTENT_TYPE, "text/event-stream")
                                    .body(StreamBody::new(stream).boxed_unsync())
                                    .unwrap();
                                Ok::<_, Infallible>(response)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
    Upstream {
        address,
        records,
        shutdown,
    }
}

struct Gateway {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
}

impl Gateway {
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn start_gateway(
    upstream: &Upstream,
    rewrite: Vec<RewriteRule>,
    models: Vec<Model>,
    recorder: bool,
) -> Gateway {
    let directory = tempfile::tempdir().unwrap();
    let logger = JsonLogger::file(&directory.path().join("gateway.log")).unwrap();
    let store = recorder.then(|| {
        Store::new(
            directory.path().join("conversations"),
            1 << 20,
            logger.clone(),
        )
    });
    let target = Target {
        base_url: upstream.url(),
        api_key: "real-key".into(),
        rewrite,
        ..Target::default()
    };
    let keys = Map::new(HashMap::from([(
        "dev".to_string(),
        vec![Entry { target, models }],
    )]));
    let state = Arc::new(AppState {
        keys: Arc::new(keys),
        forwarder: Arc::new(Forwarder::new(HashMap::new()).unwrap()),
        recorder: store,
        logger,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(server::serve(listener, state, wait_shutdown(shutdown_rx)));
    Gateway {
        address,
        shutdown,
        task,
        directory,
    }
}

async fn wait_shutdown(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build(HttpConnector::new())
}

fn request(
    method: Method,
    url: &str,
    token: &str,
    headers: &[(&'static str, &'static str)],
    body: &'static [u8],
) -> Request<Full<Bytes>> {
    let mut request = Request::new(Full::new(Bytes::from_static(body)));
    *request.method_mut() = method;
    *request.uri_mut() = url.parse().unwrap();
    if !token.is_empty() {
        request
            .headers_mut()
            .insert(AUTHORIZATION, token.parse().unwrap());
    }
    for (name, value) in headers {
        request.headers_mut().append(*name, value.parse().unwrap());
    }
    request
}

#[tokio::test]
async fn transparent_forwarding_preserves_uri_body_and_headers() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let response = client()
        .request(request(
            Method::PATCH,
            &format!("{}/arbitrary/a%2Fb?x=1&x=2&raw=%2f", gateway.url()),
            "Bearer dev",
            &[
                ("x-custom", "one"),
                ("x-custom", "two"),
                ("user-agent", "original"),
            ],
            b"opaque payload",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers()["x-upstream"], "yes");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "opaque payload"
    );

    let records = upstream.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.method, Method::PATCH);
    assert_eq!(record.uri, "/arbitrary/a%2Fb?x=1&x=2&raw=%2f");
    assert_eq!(record.body, "opaque payload");
    assert_eq!(record.headers[AUTHORIZATION], "Bearer real-key");
    assert_eq!(record.headers[HOST], upstream.address.to_string().as_str());
    assert_eq!(record.headers.get_all("x-custom").iter().count(), 2);
    assert_eq!(record.headers["user-agent"], "original");
    assert!(record.headers.get("x-forwarded-for").is_none());
    assert!(record.headers.get("accept-encoding").is_none());

    gateway.stop().await;
}

#[tokio::test]
async fn path_rewrite_and_model_alias_reach_the_provider() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(
        &upstream,
        vec![RewriteRule {
            source: "/api".into(),
            destination: "/v1".into(),
        }],
        vec![Model {
            id: "alias".into(),
            name: "real-model".into(),
            ..Model::default()
        }],
        false,
    )
    .await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/api/chat/completions?stream=true", gateway.url()),
            "Bearer dev",
            &[("content-type", "application/json")],
            br#"{"model":"alias","n":1}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _ = response.into_body().collect().await.unwrap();
    let records = upstream.records();
    assert_eq!(records[0].uri, "/v1/chat/completions?stream=true");
    assert_eq!(
        records[0].body.as_ref(),
        &br#"{"model":"real-model","n":1}"#[..]
    );
    gateway.stop().await;
}

#[tokio::test]
async fn authentication_model_and_transport_errors() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(
        &upstream,
        vec![],
        vec![Model {
            id: "known".into(),
            name: "real".into(),
            ..Model::default()
        }],
        false,
    )
    .await;

    let response = client()
        .request(request(Method::GET, &gateway.url(), "", &[], b""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.records().len(), 0);

    let response = client()
        .request(request(
            Method::POST,
            &gateway.url(),
            "Bearer dev",
            &[("content-type", "application/json")],
            br#"{"model":"unknown"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(upstream.records().len(), 0);

    gateway.stop().await;

    let dead = Target {
        base_url: "http://127.0.0.1:1".into(),
        ..Target::default()
    };
    let keys = Map::new(HashMap::from([(
        "dev".to_string(),
        vec![Entry {
            target: dead,
            models: vec![],
        }],
    )]));
    let directory = tempfile::tempdir().unwrap();
    let logger = JsonLogger::file(&directory.path().join("gateway.log")).unwrap();
    let state = Arc::new(AppState {
        keys: Arc::new(keys),
        forwarder: Arc::new(Forwarder::new(HashMap::new()).unwrap()),
        recorder: None,
        logger,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(server::serve(listener, state, wait_shutdown(shutdown_rx)));
    let response = client()
        .request(request(
            Method::GET,
            &format!("http://{address}/v1/ping"),
            "Bearer dev",
            &[],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "Bad Gateway\n"
    );
    let _ = shutdown.send(true);
    let _ = task.await;
}

#[tokio::test]
async fn sse_first_chunk_arrives_before_upstream_finishes() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let (first_sent_tx, first_sent_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let upstream = start_sse_upstream(first_sent_tx, release_rx).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/chat/completions", gateway.url()),
            "Bearer dev",
            &[],
            br#"{"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    first_sent_rx.await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("first SSE frame should be flushed")
        .unwrap()
        .unwrap();
    assert_eq!(
        first.into_data().unwrap(),
        Bytes::from_static(b"data: first\n\n")
    );
    let _ = release_tx.send(());
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if let Ok(data) = frame.into_data() {
            rest.extend_from_slice(&data);
        }
    }
    assert_eq!(rest, b"data: last\n\n");
    gateway.stop().await;
}

/// Anthropic clients configure `base_url=.../anthropic` and the SDK appends
/// `/v1/messages`. The gateway must rewrite that prefix, resolve the alias, and
/// forward the client's Anthropic-specific headers untouched.
#[tokio::test]
async fn anthropic_messages_request_is_routed_without_protocol_changes() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(
        &upstream,
        vec![RewriteRule {
            source: "/anthropic".into(),
            destination: String::new(),
        }],
        vec![Model {
            id: "alias".into(),
            name: "claude-real".into(),
            ..Model::default()
        }],
        false,
    )
    .await;
    let request_body =
        br#"{"model":"alias","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/anthropic/v1/messages", gateway.url()),
            "Bearer dev",
            &[
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
                ("x-api-key", "client-side-value"),
            ],
            request_body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let records = upstream.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.uri, "/v1/messages");
    assert_eq!(
        record.body.as_ref(),
        &br#"{"model":"claude-real","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#
            [..]
    );
    // Anthropic's own protocol headers are application-layer and pass through.
    assert_eq!(record.headers["anthropic-version"], "2023-06-01");
    assert_eq!(record.headers["x-api-key"], "client-side-value");
    assert_eq!(record.headers[AUTHORIZATION], "Bearer real-key");
    gateway.stop().await;
}

/// The whole Anthropic event vocabulary must arrive verbatim, in order, with
/// the first event flushed before the upstream has produced the rest. A frame
/// split across two upstream writes must not be held back waiting for the
/// trailing newline pair.
#[tokio::test]
async fn anthropic_sse_events_stream_through_unchanged() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let (first_sent_tx, first_sent_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let upstream = start_anthropic_sse_upstream(first_sent_tx, release_rx).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/messages", gateway.url()),
            "Bearer dev",
            &[
                ("content-type", "application/json"),
                ("anthropic-version", "2023-06-01"),
            ],
            br#"{"model":"claude-real","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[CONTENT_TYPE],
        "text/event-stream",
        "streaming content type must be preserved"
    );

    let mut body = response.into_body();
    first_sent_rx.await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("message_start should be flushed before the upstream finishes")
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap().as_ref(), ANTHROPIC_MESSAGE_START);

    let _ = release_tx.send(());
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            rest.extend_from_slice(&data);
        }
    }
    let mut expected = Vec::new();
    for chunk in [
        ANTHROPIC_BLOCK_START,
        ANTHROPIC_DELTA_HEAD,
        ANTHROPIC_DELTA_TAIL,
        ANTHROPIC_BLOCK_STOP,
        ANTHROPIC_MESSAGE_DELTA,
        ANTHROPIC_MESSAGE_STOP,
    ] {
        expected.extend_from_slice(chunk);
    }
    assert_eq!(rest, expected, "every event byte must survive the proxy");

    // The alias in the request was not rewritten (id == name is not required to
    // be equal to pass through; no alias was configured for this model).
    let records = upstream.records();
    assert_eq!(records[0].uri, "/v1/messages");
    assert_eq!(records[0].headers["anthropic-version"], "2023-06-01");
    gateway.stop().await;
}

/// Upstream failures keep their status, content type and Anthropic-shaped body.
#[tokio::test]
async fn anthropic_error_response_is_relayed_verbatim() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    const ERROR: &[u8] = br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"},"request_id":"req_01"}"#;
    let upstream =
        start_static_upstream(StatusCode::TOO_MANY_REQUESTS, "application/json", ERROR).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/messages", gateway.url()),
            "Bearer dev",
            &[("content-type", "application/json")],
            br#"{"model":"claude-real","max_tokens":8,"messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        ERROR
    );
    gateway.stop().await;
}

/// A non-streaming Anthropic reply keeps its nested `message.model` — the
/// gateway does not rewrite response payloads.
#[tokio::test]
async fn anthropic_message_response_restores_the_resolved_model_id() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    const REPLY: &[u8] = br#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-real-20261101","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#;
    let upstream = start_static_upstream(StatusCode::OK, "application/json", REPLY).await;
    let gateway = start_gateway(
        &upstream,
        vec![],
        vec![Model {
            id: "alias".into(),
            name: "claude-real".into(),
            ..Model::default()
        }],
        false,
    )
    .await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/messages", gateway.url()),
            "Bearer dev",
            &[("content-type", "application/json")],
            br#"{"model":"alias","max_tokens":8,"messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("content-length"));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let expected = std::str::from_utf8(REPLY)
        .unwrap()
        .replace("claude-real-20261101", "alias");
    assert_eq!(body.as_ref(), expected.as_bytes());
    // The request was still rewritten to the upstream name.
    assert_eq!(
        upstream.records()[0].body.as_ref(),
        &br#"{"model":"claude-real","max_tokens":8,"messages":[]}"#[..]
    );
    gateway.stop().await;
}

/// A bodyless model-list request is answered by the gateway for the
/// authenticated key: the path is recognized after rewriting, the declared ids
/// are returned in the Anthropic shape, and no provider is contacted.
#[tokio::test]
async fn anthropic_model_listing_is_served_locally() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(
        &upstream,
        vec![RewriteRule {
            source: "/anthropic".into(),
            destination: String::new(),
        }],
        vec![
            Model {
                id: "mimo-flash".into(),
                name: "real-flash".into(),
                ..Model::default()
            },
            Model {
                id: "mimo-pro".into(),
                name: "real-pro".into(),
                ..Model::default()
            },
        ],
        false,
    )
    .await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/anthropic/v1/models", gateway.url()),
            "Bearer dev",
            &[("anthropic-version", "2023-06-01")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["has_more"], false);
    assert_eq!(body["data"][0]["type"], "model");
    let ids: Vec<_> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mimo-flash", "mimo-pro"]);
    assert_eq!(upstream.records().len(), 0, "the provider is not contacted");
    gateway.stop().await;
}

/// Claude Code warms a connection by probing `/api/hello` under its base
/// URL. The gateway answers it after rewriting, with an empty 200, so the
/// provider is never contacted.
#[tokio::test]
async fn anthropic_warmup_probe_is_answered_locally() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(
        &upstream,
        vec![RewriteRule {
            source: "/anthropic".into(),
            destination: String::new(),
        }],
        vec![],
        false,
    )
    .await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/anthropic/api/hello", gateway.url()),
            "Bearer dev",
            &[("anthropic-version", "2023-06-01")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty());
    assert_eq!(upstream.records().len(), 0, "the provider is not contacted");
    gateway.stop().await;
}

#[tokio::test]
async fn conversation_record_contains_request_and_response() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_echo_upstream().await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/chat?secret=query", gateway.url()),
            "Bearer dev",
            &[("content-type", "application/json")],
            br#"{"n":1}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _ = response.into_body().collect().await.unwrap();

    let root = gateway.directory.path().join("conversations");
    let mut files = Vec::new();
    for day in std::fs::read_dir(&root).unwrap() {
        for file in std::fs::read_dir(day.unwrap().path()).unwrap() {
            files.push(file.unwrap().path());
        }
    }
    assert_eq!(files.len(), 1, "one file per authenticated call");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(record["method"], "POST");
    assert_eq!(record["path"], "/v1/chat");
    assert_eq!(record["request"]["data"], r#"{"n":1}"#);
    assert_eq!(record["response"]["data"], r#"{"n":1}"#);
    assert!(
        record["response_complete"].as_bool().unwrap(),
        "record: {record}"
    );
    // The query string and credentials are not copied into metadata.
    let text = std::fs::read_to_string(&files[0]).unwrap();
    assert!(!text.contains("secret=query"));
    assert!(!text.contains("real-key"));
    gateway.stop().await;
}

const CODED_PLAIN: &[u8] =
    br#"{"id":"msg_01","type":"message","content":[{"type":"text","text":"OK"}]}"#;

fn conversation_records(gateway: &Gateway) -> Vec<serde_json::Value> {
    let root = gateway.directory.path().join("conversations");
    let mut files = Vec::new();
    for day in std::fs::read_dir(&root).unwrap() {
        for file in std::fs::read_dir(day.unwrap().path()).unwrap() {
            files.push(file.unwrap().path());
        }
    }
    files
        .iter()
        .map(|file| serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn compressed_response_is_delivered_as_plaintext() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/messages", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip, br")],
            br#"{"model":"m","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("content-encoding").is_none(),
        "decoded response must not name a coding"
    );
    assert!(
        response.headers().get("content-length").is_none(),
        "decoded response must not carry the encoded length"
    );
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        CODED_PLAIN
    );
    assert_eq!(upstream.records()[0].headers["accept-encoding"], "gzip, br");
    gateway.stop().await;
}

#[tokio::test]
async fn brotli_only_client_gets_plaintext() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "br")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        CODED_PLAIN
    );
    assert_eq!(upstream.records()[0].headers["accept-encoding"], "br");
    gateway.stop().await;
}

#[tokio::test]
async fn unsupported_client_coding_is_not_advertised_upstream() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "compress, exi")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        CODED_PLAIN
    );
    assert!(
        upstream.records()[0]
            .headers
            .get("accept-encoding")
            .is_none(),
        "an empty intersection removes the header"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn missing_accept_encoding_is_not_added_upstream() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        upstream.records()[0]
            .headers
            .get("accept-encoding")
            .is_none(),
        "the gateway must not invent an Accept-Encoding header"
    );
    gateway.stop().await;
}

#[tokio::test]
async fn wildcard_and_quality_values_reduce_to_the_supported_set() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;

    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "*")],
            b"",
        ))
        .await
        .unwrap();
    let _ = response.into_body().collect().await.unwrap();
    assert_eq!(
        upstream.records()[0].headers["accept-encoding"],
        "gzip, deflate, br, zstd"
    );

    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip;q=0, deflate")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.records()[1].headers["accept-encoding"], "deflate");
    gateway.stop().await;
}

#[tokio::test]
async fn gzip_sse_first_event_arrives_before_the_rest() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let (first_sent_tx, first_sent_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let upstream = start_gzip_sse_upstream(first_sent_tx, release_rx).await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let response = client()
        .request(request(
            Method::POST,
            &format!("{}/v1/chat/completions", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            br#"{"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("content-encoding").is_none());
    let mut body = response.into_body();
    first_sent_rx.await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("the decoded first event should be flushed before the upstream finishes")
        .unwrap()
        .unwrap();
    assert_eq!(
        first.into_data().unwrap(),
        Bytes::from_static(b"data: first\n\n")
    );
    let _ = release_tx.send(());
    let mut rest = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            rest.extend_from_slice(&data);
        }
    }
    assert_eq!(rest, b"data: last\n\n");
    assert_eq!(upstream.records()[0].headers["accept-encoding"], "gzip");
    let record = &conversation_records(&gateway)[0];
    assert_eq!(record["response"]["data"], "data: first\n\ndata: last\n\n");
    assert_eq!(record["response"]["encoding"], "utf-8");
    assert_eq!(record["response"]["observed_bytes"], 25);
    assert_eq!(record["response_complete"], true);
    gateway.stop().await;
}

#[tokio::test]
async fn unsupported_response_coding_is_rejected_before_forwarding_bytes() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream =
        start_coding_upstream(CodingMode::Declare("compress"), StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "Bad Gateway\n"
    );
    let record = &conversation_records(&gateway)[0];
    assert_eq!(record["error"], "upstream_encoding");
    gateway.stop().await;
}

#[tokio::test]
async fn layered_response_coding_is_rejected() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream =
        start_coding_upstream(CodingMode::Declare("gzip, br"), StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip, br")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    gateway.stop().await;
}

#[tokio::test]
async fn bodyless_responses_keep_their_coding_headers() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream =
        start_coding_upstream(CodingMode::Force("gzip"), StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::HEAD,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    gateway.stop().await;

    let upstream =
        start_coding_upstream(CodingMode::Declare("gzip"), StatusCode::NOT_MODIFIED, b"").await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    gateway.stop().await;
}

#[tokio::test]
async fn empty_body_with_a_coding_is_delivered_empty() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Declare("gzip"), StatusCode::OK, b"").await;
    let gateway = start_gateway(&upstream, vec![], vec![], false).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("content-encoding").is_none());
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    gateway.stop().await;
}

#[tokio::test]
async fn conversation_record_contains_decoded_response_and_raw_request() {
    use base64::Engine as _;

    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let upstream = start_coding_upstream(CodingMode::Negotiated, StatusCode::OK, CODED_PLAIN).await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let raw_request = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let compressed_request = encode_body("gzip", raw_request).await;
    let mut forwarded = Request::new(Full::new(Bytes::from(compressed_request.clone())));
    *forwarded.method_mut() = Method::POST;
    *forwarded.uri_mut() = format!("{}/v1/messages", gateway.url()).parse().unwrap();
    forwarded
        .headers_mut()
        .insert(AUTHORIZATION, "Bearer dev".parse().unwrap());
    forwarded
        .headers_mut()
        .insert("content-encoding", "gzip".parse().unwrap());
    forwarded
        .headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
    let response = client().request(forwarded).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("content-encoding").is_none());
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        CODED_PLAIN
    );

    let record = &conversation_records(&gateway)[0];
    assert_eq!(
        record["response"]["data"],
        String::from_utf8_lossy(CODED_PLAIN).to_string()
    );
    assert_eq!(record["response"]["encoding"], "utf-8");
    assert_eq!(record["response"]["observed_bytes"], CODED_PLAIN.len());
    assert_eq!(record["response_complete"], true);
    // The request body is recorded as it arrived, still compressed.
    assert_eq!(record["request"]["encoding"], "base64");
    assert_eq!(
        record["request"]["observed_bytes"],
        compressed_request.len()
    );
    let captured = base64::engine::general_purpose::STANDARD
        .decode(record["request"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(captured, compressed_request);
    // The upstream saw the compressed request unchanged.
    assert_eq!(upstream.records()[0].body.as_ref(), &compressed_request[..]);
    gateway.stop().await;
}

#[tokio::test]
async fn binary_decoded_response_is_recorded_as_base64() {
    use base64::Engine as _;

    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let mut binary: Vec<u8> = (0u8..=255).collect();
    binary.extend_from_slice(&[0x00, 0xff, 0xfe, 0x80]);
    let plain: &'static [u8] = Box::leak(binary.into_boxed_slice());
    let upstream = start_coding_upstream(CodingMode::Force("gzip"), StatusCode::OK, plain).await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    let response = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        plain
    );
    let record = &conversation_records(&gateway)[0];
    assert_eq!(record["response"]["encoding"], "base64");
    assert_eq!(record["response"]["observed_bytes"], plain.len());
    let captured = base64::engine::general_purpose::STANDARD
        .decode(record["response"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(captured, plain.to_vec());
    gateway.stop().await;
}

#[tokio::test]
async fn truncated_gzip_marks_the_record_incomplete() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let plain: &'static [u8] = Box::leak(
        format!("{{\"data\":\"{}\"}}", "all work and no play ".repeat(200))
            .into_bytes()
            .into_boxed_slice(),
    );
    let upstream = start_coding_upstream(CodingMode::TruncatedGzip, StatusCode::OK, plain).await;
    let gateway = start_gateway(&upstream, vec![], vec![], true).await;
    // Depending on how early the decoder notices, the client either receives
    // headers and then a broken body stream, or loses the connection outright.
    // Both prove the truncated reply is not delivered as complete.
    if let Ok(response) = client()
        .request(request(
            Method::GET,
            &format!("{}/v1/ping", gateway.url()),
            "Bearer dev",
            &[("accept-encoding", "gzip")],
            b"",
        ))
        .await
    {
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.into_body().collect().await.is_err(),
            "a truncated coded body must not look like a complete reply"
        );
    }
    let record = &conversation_records(&gateway)[0];
    assert_eq!(record["error"], "upstream_decode");
    assert_eq!(record["response_complete"], false);
    gateway.stop().await;
}

const RESPONSE_JSON_LIMIT: usize = 32 << 20;
const RESPONSE_SSE_LIMIT: usize = 1 << 20;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

struct ResponseScript {
    content_type: &'static str,
    gzip: bool,
    content_length: Option<usize>,
    clean: bool,
}

impl ResponseScript {
    fn chunked(content_type: &'static str) -> Self {
        Self {
            content_type,
            gzip: false,
            content_length: None,
            clean: true,
        }
    }
}

/// A loopback upstream whose headers arrive immediately and whose data writes
/// and termination are controlled by the test.
async fn start_scripted_responses(
    scripts: Vec<ResponseScript>,
) -> (Upstream, Vec<tokio::sync::mpsc::Sender<Bytes>>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let records = Arc::new(Mutex::new(Vec::new()));
    let (senders, receivers): (Vec<_>, Vec<_>) =
        scripts.iter().map(|_| mpsc::channel::<Bytes>(2)).unzip();
    let service_records = records.clone();

    tokio::spawn(async move {
        for (script, mut receiver) in scripts.into_iter().zip(receivers) {
            let (stream, _) = tokio::select! {
                _ = shutdown_rx.changed() => return,
                connection = listener.accept() => connection.unwrap(),
            };
            let records = service_records.clone();
            tokio::spawn(async move {
                let mut stream = BufReader::new(stream);
                let mut first_line = String::new();
                stream.read_line(&mut first_line).await.unwrap();
                let mut first_line = first_line.split_whitespace();
                let method = first_line.next().unwrap().parse().unwrap();
                let uri = first_line.next().unwrap().to_string();
                let mut headers = HeaderMap::new();
                loop {
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.append(
                        name.parse::<http::HeaderName>().unwrap(),
                        value.trim().parse().unwrap(),
                    );
                }
                let length = headers["content-length"].to_str().unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                records.lock().unwrap().push(Recorded {
                    method,
                    uri,
                    headers,
                    body: body.into(),
                });
                let mut stream = stream.into_inner();

                let framing = match script.content_length {
                    Some(length) => format!("Content-Length: {length}\r\n"),
                    None => "Transfer-Encoding: chunked\r\n".to_string(),
                };
                let encoding = if script.gzip {
                    "Content-Encoding: gzip\r\n"
                } else {
                    ""
                };
                let content_type = script.content_type;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{framing}{encoding}ETag: \"original\"\r\nContent-MD5: original\r\nContent-Digest: original\r\nX-Keep: yes\r\n\r\n"
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                let chunked = script.content_length.is_none();
                while let Some(bytes) = receiver.recv().await {
                    if chunked {
                        stream
                            .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    stream.write_all(&bytes).await.unwrap();
                    if chunked {
                        stream.write_all(b"\r\n").await.unwrap();
                    }
                }
                if chunked && script.clean {
                    stream.write_all(b"0\r\n\r\n").await.unwrap();
                }
                stream.shutdown().await.unwrap();
            });
        }
    });
    (
        Upstream {
            address,
            records,
            shutdown,
        },
        senders,
    )
}

async fn scripted_response(script: ResponseScript) -> (Upstream, tokio::sync::mpsc::Sender<Bytes>) {
    let (upstream, mut senders) = start_scripted_responses(vec![script]).await;
    (upstream, senders.pop().unwrap())
}

fn response_mapping(id: &str, name: &str) -> Model {
    Model {
        id: id.into(),
        name: name.into(),
        ..Model::default()
    }
}

async fn model_request(gateway: &Gateway, path: &str, model: &str) -> Response<Incoming> {
    let mut request = request(
        Method::POST,
        &format!("{}{path}", gateway.url()),
        "Bearer dev",
        &[
            ("content-type", "application/json"),
            ("accept-encoding", "gzip"),
        ],
        b"",
    );
    *request.body_mut() = Full::new(Bytes::from(format!(r#"{{"model":"{model}"}}"#)));
    tokio::time::timeout(RESPONSE_TIMEOUT, client().request(request))
        .await
        .unwrap()
        .unwrap()
}

async fn response_bytes(body: Incoming) -> Bytes {
    tokio::time::timeout(RESPONSE_TIMEOUT, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes()
}

async fn read_response_prefix(body: &mut Incoming, len: usize) -> Vec<u8> {
    tokio::time::timeout(RESPONSE_TIMEOUT, async {
        let mut bytes = Vec::new();
        while bytes.len() < len {
            let frame = body.frame().await.unwrap().unwrap();
            bytes.extend_from_slice(frame.data_ref().unwrap());
        }
        bytes
    })
    .await
    .unwrap()
}

async fn read_before_body_error(mut body: Incoming) -> Vec<u8> {
    tokio::time::timeout(RESPONSE_TIMEOUT, async {
        let mut bytes = Vec::new();
        loop {
            match body.frame().await {
                Some(Ok(frame)) => bytes.extend_from_slice(frame.data_ref().unwrap()),
                Some(Err(_)) => return bytes,
                None => panic!("incomplete upstream response ended cleanly"),
            }
        }
    })
    .await
    .unwrap()
}

type StreamingGzip = async_compression::tokio::write::GzipEncoder<Vec<u8>>;

async fn gzip_chunk(encoder: &mut StreamingGzip, bytes: &[u8]) -> Bytes {
    use tokio::io::AsyncWriteExt;
    encoder.write_all(bytes).await.unwrap();
    encoder.flush().await.unwrap();
    std::mem::take(encoder.get_mut()).into()
}

async fn gzip_finish(encoder: &mut StreamingGzip) -> Bytes {
    use tokio::io::AsyncWriteExt;
    encoder.shutdown().await.unwrap();
    std::mem::take(encoder.get_mut()).into()
}

type MatchedResponseCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [u8],
    &'static [u8],
);

#[tokio::test]
async fn matched_response_ids_cover_protocols_suffixes_and_same_name_mappings() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let cases: [MatchedResponseCase; 4] = [
        (
            "/v1/chat/completions", "application/json", "same", "same",
            br#"{ "model" : "same-20261101", "n":1e+2 }"#,
            br#"{ "model" : "same", "n":1e+2 }"#,
        ),
        (
            "/v1/messages", "application/json", "alias[1m]", "provider[1m]",
            br#"{"model":"provider-20261101","content":[]}"#,
            br#"{"model":"alias[1m]","content":[]}"#,
        ),
        (
            "/v1/chat/completions", "text/event-stream", "alias[1m]", "provider[1m]",
            b"data: {\"model\":\"provider-20261101\"}\n\ndata: {\"model\":null}\n\ndata: {}\n\ndata: [DONE]\n\n",
            b"data: {\"model\":\"alias[1m]\"}\n\ndata: {\"model\":null}\n\ndata: {}\n\ndata: [DONE]\n\n",
        ),
        (
            "/anthropic/messages", "text/event-stream", "same", "same",
            b"event: message_start\ndata: {\"message\":{\"model\":\"same-20261101\"}}\n\nevent: message_start\ndata: {\"message\":{\"model\":5}}\n\nevent: message_start\ndata: {\"message\":{}}\n\n",
            b"event: message_start\ndata: {\"message\":{\"model\":\"same\"}}\n\nevent: message_start\ndata: {\"message\":{\"model\":5}}\n\nevent: message_start\ndata: {\"message\":{}}\n\n",
        ),
    ];
    for (path, content_type, id, upstream_name, reply, expected) in cases {
        let upstream = start_static_upstream(StatusCode::OK, content_type, reply).await;
        let gateway = start_gateway(
            &upstream,
            vec![RewriteRule {
                source: "/anthropic".into(),
                destination: "/v1".into(),
            }],
            vec![
                response_mapping("same", "same"),
                response_mapping("alias", "provider"),
            ],
            false,
        )
        .await;
        let response = model_request(&gateway, path, id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_bytes(response.into_body()).await, expected);
        assert_eq!(
            upstream.records()[0].body,
            format!(r#"{{"model":"{upstream_name}"}}"#)
        );
        gateway.stop().await;
    }
}

#[tokio::test]
async fn overlapping_responses_restore_their_own_alias_in_reverse_order() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let (upstream, mut senders) = start_scripted_responses(vec![
        ResponseScript::chunked("application/json"),
        ResponseScript::chunked("application/json"),
    ])
    .await;
    let second = senders.pop().unwrap();
    let first = senders.pop().unwrap();
    let gateway = start_gateway(
        &upstream,
        vec![],
        vec![
            response_mapping("first", "provider"),
            response_mapping("second", "provider"),
        ],
        false,
    )
    .await;
    let path = "/v1/chat/completions";
    let response_first = model_request(&gateway, path, "first").await;
    let response_second = model_request(&gateway, path, "second").await;
    let provider = Bytes::from_static(br#"{"model":"provider-20261101"}"#);
    second.send(provider.clone()).await.unwrap();
    drop(second);
    assert_eq!(
        response_bytes(response_second.into_body()).await,
        br#"{"model":"second"}"#[..]
    );
    first.send(provider).await.unwrap();
    drop(first);
    assert_eq!(
        response_bytes(response_first.into_body()).await,
        br#"{"model":"first"}"#[..]
    );
    assert!(
        upstream
            .records()
            .iter()
            .all(|r| r.body == br#"{"model":"provider"}"#[..])
    );
    gateway.stop().await;
}

#[tokio::test]
async fn inspected_sse_flushes_split_and_coalesced_events_before_eof() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    for gzip in [false, true] {
        let (upstream, sender) = scripted_response(ResponseScript {
            gzip,
            ..ResponseScript::chunked("Text/Event-Stream; charset=utf-8")
        })
        .await;
        let gateway = start_gateway(
            &upstream,
            vec![],
            vec![response_mapping("alias", "real")],
            true,
        )
        .await;
        let response = model_request(&gateway, "/v1/chat/completions", "alias").await;
        for name in [
            "content-length",
            "content-encoding",
            "content-md5",
            "content-digest",
            "etag",
        ] {
            assert!(!response.headers().contains_key(name), "{name}");
        }
        assert_eq!(response.headers()["x-keep"], "yes");
        let mut body = response.into_body();
        let mut encoder = StreamingGzip::new(Vec::new());
        for chunk in [
            &b"data: {\"mod"[..],
            b"el\":\"real\"}\n\ndata: {\"model\":\"real\"}\n\n",
        ] {
            let wire = if gzip {
                gzip_chunk(&mut encoder, chunk).await
            } else {
                Bytes::from_static(chunk)
            };
            sender.send(wire).await.unwrap();
        }
        let expected = b"data: {\"model\":\"alias\"}\n\ndata: {\"model\":\"alias\"}\n\n";
        assert_eq!(
            read_response_prefix(&mut body, expected.len()).await,
            expected
        );
        if gzip {
            sender.send(gzip_finish(&mut encoder).await).await.unwrap();
        }
        drop(sender);
        assert!(response_bytes(body).await.is_empty());
        let record = &conversation_records(&gateway)[0];
        assert_eq!(
            record["response"]["data"],
            std::str::from_utf8(expected).unwrap()
        );
        assert_eq!(record["response_complete"], true);
        gateway.stop().await;
    }
}

#[tokio::test]
async fn inspected_json_waits_for_eof_and_handles_split_model_fields() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    for gzip in [false, true] {
        let (upstream, sender) = scripted_response(ResponseScript {
            gzip,
            ..ResponseScript::chunked("application/json; charset=utf-8")
        })
        .await;
        let gateway = start_gateway(
            &upstream,
            vec![],
            vec![response_mapping("alias", "real")],
            false,
        )
        .await;
        let mut body = model_request(&gateway, "/v1/messages", "alias")
            .await
            .into_body();
        let mut encoder = StreamingGzip::new(Vec::new());
        // Exercise exactly 32 MiB of decoded data in the identity case.
        let mut payload = br#"{ "model" : "real", "n": 1e999 }"#.to_vec();
        if !gzip {
            payload.resize(RESPONSE_JSON_LIMIT, b' ');
        }
        for chunk in [&payload[..6], &payload[6..15], &payload[15..]] {
            let wire = if gzip {
                gzip_chunk(&mut encoder, chunk).await
            } else {
                Bytes::copy_from_slice(chunk)
            };
            sender.send(wire).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), body.frame())
                .await
                .is_err()
        );
        if gzip {
            sender.send(gzip_finish(&mut encoder).await).await.unwrap();
        }
        drop(sender);
        let mut expected = br#"{ "model" : "alias", "n": 1e999 }"#.to_vec();
        if !gzip {
            expected.resize(RESPONSE_JSON_LIMIT + 1, b' ');
        }
        assert_eq!(response_bytes(body).await, expected);
        gateway.stop().await;
    }
}

#[tokio::test]
async fn oversized_json_forwards_before_eof_with_known_or_discovered_length() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    for (known, gzip) in [(true, false), (false, false), (false, true)] {
        let mut payload = br#"{"model":"real","n":1}"#.to_vec();
        let prefix_len = if known {
            payload.len()
        } else {
            RESPONSE_JSON_LIMIT + 1
        };
        payload.resize(RESPONSE_JSON_LIMIT + 1, b' ');
        let final_bytes = b"   ";
        let (upstream, sender) = scripted_response(ResponseScript {
            content_length: known.then_some(payload.len() + final_bytes.len()),
            gzip,
            ..ResponseScript::chunked("application/json")
        })
        .await;
        let gateway = start_gateway(
            &upstream,
            vec![],
            vec![response_mapping("alias", "real")],
            false,
        )
        .await;
        let response = model_request(&gateway, "/v1/messages", "alias").await;
        assert_eq!(response.headers().contains_key("content-length"), known);
        assert_eq!(response.headers().contains_key("etag"), known);
        let mut body = response.into_body();
        let mut encoder = StreamingGzip::new(Vec::new());
        let prefix = &payload[..prefix_len];
        let wire = if gzip {
            gzip_chunk(&mut encoder, prefix).await
        } else {
            Bytes::copy_from_slice(prefix)
        };
        sender.send(wire).await.unwrap();
        assert_eq!(read_response_prefix(&mut body, prefix_len).await, prefix);

        let tail = &payload[prefix_len..];
        if !tail.is_empty() {
            sender.send(Bytes::copy_from_slice(tail)).await.unwrap();
        }
        let wire = if gzip {
            gzip_chunk(&mut encoder, final_bytes).await
        } else {
            Bytes::from_static(final_bytes)
        };
        sender.send(wire).await.unwrap();
        if gzip {
            sender.send(gzip_finish(&mut encoder).await).await.unwrap();
        }
        drop(sender);
        let received_tail = response_bytes(body).await;
        assert_eq!(&received_tail[..tail.len()], tail);
        assert_eq!(&received_tail[tail.len()..], final_bytes);
        gateway.stop().await;
    }
}

#[tokio::test]
async fn inspected_json_and_sse_preserve_partial_bytes_on_eof_and_body_failures() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let cases: [(&str, &[u8]); 3] = [
        ("application/json", br#"{"model":"real""#),
        ("application/json", br#"{"model":"real"}"#),
        ("text/event-stream", b"data: {\"model\":\"real\"}\n"),
    ];
    for (content_type, partial) in cases {
        for failure in [None, Some("upstream_read"), Some("upstream_decode")] {
            let gzip = failure == Some("upstream_decode");
            let (upstream, sender) = scripted_response(ResponseScript {
                gzip,
                clean: failure != Some("upstream_read"),
                ..ResponseScript::chunked(content_type)
            })
            .await;
            let gateway = start_gateway(
                &upstream,
                vec![],
                vec![response_mapping("alias", "real")],
                true,
            )
            .await;
            let body = model_request(&gateway, "/v1/chat/completions", "alias")
                .await
                .into_body();
            let wire = if gzip {
                // A sync flush yields the plaintext, but no gzip trailer.
                gzip_chunk(&mut StreamingGzip::new(Vec::new()), partial).await
            } else {
                Bytes::from_static(partial)
            };
            sender.send(wire).await.unwrap();
            drop(sender);
            let expected: &[u8] = if failure.is_none() && partial == br#"{"model":"real"}"# {
                br#"{"model":"alias"}"#
            } else {
                partial
            };
            let received = if failure.is_some() {
                read_before_body_error(body).await
            } else {
                response_bytes(body).await.to_vec()
            };
            assert_eq!(received, expected, "{content_type}, {failure:?}");
            let record = &conversation_records(&gateway)[0];
            assert_eq!(
                record["response"]["data"],
                std::str::from_utf8(expected).unwrap()
            );
            assert_eq!(record["response_complete"], failure.is_none());
            if let Some(category) = failure {
                assert_eq!(record["error"], category);
            }
            gateway.stop().await;
        }
    }
}

#[tokio::test]
async fn oversized_sse_forwards_before_delimiter_and_resumes_rewriting() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    for gzip in [false, true] {
        let (upstream, sender) = scripted_response(ResponseScript {
            gzip,
            ..ResponseScript::chunked("text/event-stream")
        })
        .await;
        let gateway = start_gateway(
            &upstream,
            vec![],
            vec![response_mapping("alias", "real")],
            false,
        )
        .await;
        let mut body = model_request(&gateway, "/v1/chat/completions", "alias")
            .await
            .into_body();
        let mut encoder = StreamingGzip::new(Vec::new());

        let mut prefix = b"data: {\"model\":\"real\"}".to_vec();
        prefix.resize(RESPONSE_SSE_LIMIT + 1, b' ');
        let wire = if gzip {
            gzip_chunk(&mut encoder, &prefix).await
        } else {
            Bytes::from(prefix.clone())
        };
        sender.send(wire).await.unwrap();
        assert_eq!(read_response_prefix(&mut body, prefix.len()).await, prefix);

        for plaintext in [&b"\r\n\r"[..], b"\ndata: {\"model\":\"real\"}\r\n\r\n"] {
            let wire = if gzip {
                gzip_chunk(&mut encoder, plaintext).await
            } else {
                Bytes::from_static(plaintext)
            };
            sender.send(wire).await.unwrap();
        }
        let expected = b"\r\n\r\ndata: {\"model\":\"alias\"}\r\n\r\n";
        assert_eq!(
            read_response_prefix(&mut body, expected.len()).await,
            expected
        );
        if gzip {
            sender.send(gzip_finish(&mut encoder).await).await.unwrap();
        }
        drop(sender);
        assert!(response_bytes(body).await.is_empty());
        gateway.stop().await;
    }
}

type PassthroughResponseCase = (
    StatusCode,
    &'static str,
    Vec<Model>,
    &'static str,
    &'static [u8],
);

#[tokio::test]
async fn response_restoration_preserves_provider_errors_unmatched_and_other_endpoints() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let eligible = "/v1/chat/completions";
    let cases: [PassthroughResponseCase; 6] = [
        (
            StatusCode::BAD_REQUEST,
            eligible,
            vec![response_mapping("alias", "real")],
            "alias",
            br#"{"model":"real","error":"invalid"}"#,
        ),
        (
            StatusCode::OK,
            eligible,
            vec![],
            "alias",
            br#"{"model":"real"}"#,
        ),
        (
            StatusCode::OK,
            "/v1/embeddings",
            vec![response_mapping("alias", "real")],
            "alias",
            br#"{"model":"real"}"#,
        ),
        (
            StatusCode::OK,
            eligible,
            vec![response_mapping("alias", "real")],
            "alias",
            br#"{"nested":{"model":"real"}}"#,
        ),
        (
            StatusCode::OK,
            "/v1/messages",
            vec![response_mapping("alias", "real")],
            "alias",
            br#"{"model":false}"#,
        ),
        (
            StatusCode::OK,
            "/v1/messages",
            vec![response_mapping("alias", "real")],
            "alias",
            br#"{"model":"alias"}"#,
        ),
    ];
    for (status, path, models, id, original) in cases {
        let upstream = start_static_upstream(status, "application/json", original).await;
        let gateway = start_gateway(&upstream, vec![], models, false).await;
        let response = model_request(&gateway, path, id).await;
        assert_eq!(response.status(), status);
        assert_eq!(response_bytes(response.into_body()).await, original);
        gateway.stop().await;
    }
}
