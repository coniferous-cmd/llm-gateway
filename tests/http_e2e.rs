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
            &format!("http://{address}/v1/models"),
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
async fn anthropic_message_response_keeps_the_upstream_model_name() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    const REPLY: &[u8] = br#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-real","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}"#;
    let upstream = start_static_upstream(StatusCode::OK, "application/json", REPLY).await;
    let gateway = start_gateway(
        &upstream,
        vec![],
        vec![Model {
            id: "alias".into(),
            name: "claude-real".into(),
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), REPLY);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["model"], "claude-real");
    // The request was still rewritten to the upstream name.
    assert_eq!(
        upstream.records()[0].body.as_ref(),
        &br#"{"model":"claude-real","max_tokens":8,"messages":[]}"#[..]
    );
    gateway.stop().await;
}

/// `GET /v1/models`-style calls without a body must still reach the provider and
/// relay its listing as-is.
#[tokio::test]
async fn anthropic_model_listing_passes_through_without_a_body() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    const LISTING: &[u8] = br#"{"data":[{"type":"model","id":"claude-real","display_name":"Claude"}],"has_more":false}"#;
    let upstream = start_static_upstream(StatusCode::OK, "application/json", LISTING).await;
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
            &format!("{}/anthropic/v1/models", gateway.url()),
            "Bearer dev",
            &[("anthropic-version", "2023-06-01")],
            b"",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        LISTING
    );
    let records = upstream.records();
    assert_eq!(records[0].method, Method::GET);
    assert_eq!(records[0].uri, "/v1/models");
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
