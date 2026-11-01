//! Two-gateway encrypted chain over loopback TCP.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::AUTHORIZATION;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use llm_gateway::config::{Entry, Target};
use llm_gateway::e2ee::{Dialer, Identity};
use llm_gateway::keymap::Map;
use llm_gateway::logging::JsonLogger;
use llm_gateway::proxy::forward::{Forwarder, PeerDialer};
use llm_gateway::server::{self, AppState};
use tokio::net::TcpListener;
use tokio::sync::watch;

fn loopback_available() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

async fn wait_shutdown(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

#[derive(Clone, Debug)]
struct Recorded {
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

async fn upstream() -> (SocketAddr, Arc<Mutex<Vec<Recorded>>>, watch::Sender<bool>) {
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
                                let (parts, body) = request.into_parts();
                                let body = body.collect().await.unwrap().to_bytes();
                                records.lock().unwrap().push(Recorded {
                                    uri: parts.uri.to_string(),
                                    headers: parts.headers,
                                    body: body.clone(),
                                });
                                let response = Response::builder()
                                    .status(StatusCode::OK)
                                    .header("x-provider", "ok")
                                    .body(
                                        Full::new(Bytes::from_static(b"provider reply"))
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
    (address, records, shutdown)
}

fn state(
    logger: Arc<JsonLogger>,
    token: &str,
    target: Target,
    peers: HashMap<String, Arc<dyn PeerDialer>>,
) -> Arc<AppState> {
    Arc::new(AppState {
        keys: Arc::new(Map::new(HashMap::from([(
            token.to_string(),
            vec![Entry {
                target,
                models: vec![],
            }],
        )]))),
        forwarder: Arc::new(Forwarder::new(peers).unwrap()),
        recorder: None,
        logger,
    })
}

#[tokio::test]
async fn chained_gateways_encrypt_the_peer_hop_and_reuse_sessions() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let logger = JsonLogger::file(&temp.path().join("gateway.log")).unwrap();
    let (upstream_address, records, _upstream_shutdown) = upstream().await;

    // Gateway C terminates the encrypted hop and calls the provider.
    let c_identity = Identity::generate().unwrap();
    let b_identity = Identity::generate().unwrap();
    let c_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c_e2ee_address = c_listener.local_addr().unwrap();
    let c_state = state(
        logger.clone(),
        "virtual-on-c",
        Target {
            base_url: format!("http://{upstream_address}"),
            api_key: "real-provider-key".into(),
            ..Target::default()
        },
        HashMap::new(),
    );
    let (c_shutdown, c_shutdown_rx) = watch::channel(false);
    let c_task = tokio::spawn(llm_gateway::e2ee::listener::serve(
        c_listener,
        c_identity.clone(),
        HashSet::from([b_identity.public()]),
        c_state,
        wait_shutdown(c_shutdown_rx),
    ));

    // Gateway B sends the peer's virtual key over the encrypted link.
    let c_address_key = c_e2ee_address.to_string();
    let dialer: Arc<dyn PeerDialer> = Arc::new(Dialer::new(
        b_identity.clone(),
        HashMap::from([(c_address_key.clone(), c_identity.public())]),
    ));
    let b_state = state(
        logger.clone(),
        "dev",
        Target {
            base_url: format!("http://{c_address_key}"),
            api_key: "virtual-on-c".into(),
            peer: "c".into(),
            ..Target::default()
        },
        HashMap::from([(c_address_key, dialer)]),
    );
    let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_address = b_listener.local_addr().unwrap();
    let (b_shutdown, b_shutdown_rx) = watch::channel(false);
    let b_task = tokio::spawn(server::serve(
        b_listener,
        b_state,
        wait_shutdown(b_shutdown_rx),
    ));

    let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let make_request = |path: &str| {
        let mut request = Request::new(Full::new(Bytes::from_static(br#"{"hello":"world"}"#)));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = format!("http://{b_address}{path}").parse().unwrap();
        request
            .headers_mut()
            .insert(AUTHORIZATION, "Bearer dev".parse().unwrap());
        request
    };

    for path in ["/v1/chat/completions", "/v1/chat/completions"] {
        let response = client.request(make_request(path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-provider"], "ok");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "provider reply"
        );
    }
    let records = records.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.uri, "/v1/chat/completions");
        assert_eq!(record.headers[AUTHORIZATION], "Bearer real-provider-key");
        assert_eq!(record.body.as_ref(), br#"{"hello":"world"}"#);
    }

    // Both requests travelled over one keep-alive connection and therefore
    // one Noise handshake. The log is flushed per line.
    let log = std::fs::read_to_string(temp.path().join("gateway.log")).unwrap();
    assert_eq!(log.matches("e2ee session established").count(), 1, "{log}");

    let _ = b_shutdown.send(true);
    let _ = c_shutdown.send(true);
    let _ = b_task.await;
    let _ = c_task.await;
}

#[tokio::test]
async fn wrong_pinned_identity_fails_the_hop() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let logger = JsonLogger::file(&temp.path().join("gateway.log")).unwrap();
    let (upstream_address, records, _upstream_shutdown) = upstream().await;

    let c_identity = Identity::generate().unwrap();
    let b_identity = Identity::generate().unwrap();
    let impostor = Identity::generate().unwrap();
    let c_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c_e2ee_address = c_listener.local_addr().unwrap();
    let c_state = state(
        logger.clone(),
        "virtual-on-c",
        Target {
            base_url: format!("http://{upstream_address}"),
            api_key: "real-provider-key".into(),
            ..Target::default()
        },
        HashMap::new(),
    );
    let (c_shutdown, c_shutdown_rx) = watch::channel(false);
    let c_task = tokio::spawn(llm_gateway::e2ee::listener::serve(
        c_listener,
        c_identity.clone(),
        HashSet::from([b_identity.public()]),
        c_state,
        wait_shutdown(c_shutdown_rx),
    ));

    let c_address_key = c_e2ee_address.to_string();
    let dialer: Arc<dyn PeerDialer> = Arc::new(Dialer::new(
        b_identity.clone(),
        HashMap::from([(c_address_key.clone(), impostor.public())]),
    ));
    let b_state = state(
        logger.clone(),
        "dev",
        Target {
            base_url: format!("http://{c_address_key}"),
            api_key: "virtual-on-c".into(),
            peer: "c".into(),
            ..Target::default()
        },
        HashMap::from([(c_address_key, dialer)]),
    );
    let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_address = b_listener.local_addr().unwrap();
    let (b_shutdown, b_shutdown_rx) = watch::channel(false);
    let b_task = tokio::spawn(server::serve(
        b_listener,
        b_state,
        wait_shutdown(b_shutdown_rx),
    ));

    let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let mut request = Request::new(Full::new(Bytes::from_static(b"{}")));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = format!("http://{b_address}/v1/chat").parse().unwrap();
    request
        .headers_mut()
        .insert(AUTHORIZATION, "Bearer dev".parse().unwrap());
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "Bad Gateway\n"
    );
    assert!(records.lock().unwrap().is_empty());

    let _ = b_shutdown.send(true);
    let _ = c_shutdown.send(true);
    let _ = b_task.await;
    let _ = c_task.await;
}

#[tokio::test]
async fn unknown_initiator_never_reaches_the_provider() {
    if !loopback_available() {
        eprintln!("skipping: loopback binding is unavailable in this environment");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let logger = JsonLogger::file(&temp.path().join("gateway.log")).unwrap();
    let (upstream_address, records, _upstream_shutdown) = upstream().await;

    let c_identity = Identity::generate().unwrap();
    let b_identity = Identity::generate().unwrap();
    let c_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let c_e2ee_address = c_listener.local_addr().unwrap();
    let c_state = state(
        logger.clone(),
        "virtual-on-c",
        Target {
            base_url: format!("http://{upstream_address}"),
            api_key: "real-provider-key".into(),
            ..Target::default()
        },
        HashMap::new(),
    );
    let (c_shutdown, c_shutdown_rx) = watch::channel(false);
    let c_task = tokio::spawn(llm_gateway::e2ee::listener::serve(
        c_listener,
        c_identity.clone(),
        HashSet::new(), // B is not trusted.
        c_state,
        wait_shutdown(c_shutdown_rx),
    ));

    let c_address_key = c_e2ee_address.to_string();
    let dialer: Arc<dyn PeerDialer> = Arc::new(Dialer::new(
        b_identity.clone(),
        HashMap::from([(c_address_key.clone(), c_identity.public())]),
    ));
    let b_state = state(
        logger.clone(),
        "dev",
        Target {
            base_url: format!("http://{c_address_key}"),
            api_key: "virtual-on-c".into(),
            peer: "c".into(),
            ..Target::default()
        },
        HashMap::from([(c_address_key, dialer)]),
    );
    let b_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b_address = b_listener.local_addr().unwrap();
    let (b_shutdown, b_shutdown_rx) = watch::channel(false);
    let b_task = tokio::spawn(server::serve(
        b_listener,
        b_state,
        wait_shutdown(b_shutdown_rx),
    ));

    let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
    let mut request = Request::new(Full::new(Bytes::from_static(b"{}")));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = format!("http://{b_address}/v1/chat").parse().unwrap();
    request
        .headers_mut()
        .insert(AUTHORIZATION, "Bearer dev".parse().unwrap());
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect()).await;
    assert!(records.lock().unwrap().is_empty());
    let log = std::fs::read_to_string(temp.path().join("gateway.log")).unwrap();
    assert!(log.contains("e2ee handshake rejected"), "{log}");

    let _ = b_shutdown.send(true);
    let _ = c_shutdown.send(true);
    let _ = b_task.await;
    let _ = c_task.await;
}
