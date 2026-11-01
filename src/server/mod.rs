//! HTTP service pipeline: authentication, preparation, forwarding, logging.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, UPGRADE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use percent_encoding::percent_decode_str;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::Model;
use crate::conversationlog::{Call, Store};
use crate::keymap::Map;
use crate::logging::{JsonLogger, fields};
use crate::proxy::forward::{BoxError as ForwardError, Forwarder};
use crate::proxy::prepare::{self, PrepareError, ReqBody};

pub struct AppState {
    pub keys: Arc<Map>,
    pub forwarder: Arc<Forwarder>,
    pub recorder: Option<Arc<Store>>,
    pub logger: Arc<JsonLogger>,
}

/// Run the plaintext HTTP listener until `shutdown` resolves, then drain
/// in-flight requests for up to ten seconds.
pub async fn serve<F>(listener: TcpListener, state: Arc<AppState>, shutdown: F)
where
    F: Future<Output = ()>,
{
    let logger = state.logger.clone();
    let (conn_shutdown, _) = watch::channel(false);
    let mut tasks: JoinSet<()> = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let state = state.clone();
                        let shutdown = conn_shutdown.subscribe();
                        tasks.spawn(async move {
                            serve_connection(stream, state, shutdown).await;
                        });
                    }
                    Err(error) => {
                        let mut event = fields();
                        event.insert("error".into(), json!(error.to_string()));
                        logger.error("accept failed", event);
                    }
                }
            }
        }
    }
    let _ = conn_shutdown.send(true);
    drain(tasks, Duration::from_secs(10)).await;
}

pub(crate) async fn serve_connection<S>(
    stream: S,
    state: Arc<AppState>,
    mut shutdown: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let logger = state.logger.clone();
    let service = service_fn(move |request| {
        let state = state.clone();
        async move { handle(state, request).await }
    });
    let mut connection = Box::pin(
        hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30))
            .keep_alive(true)
            .serve_connection(TokioIo::new(stream), service),
    );
    tokio::select! {
        result = &mut connection => {
            if let Err(error) = result {
                log_connection_error(&logger, error);
            }
        }
        _ = wait_for_shutdown(&mut shutdown) => {
            // Stop accepting new requests, close idle keep-alives, and let
            // active responses finish for up to the drain window.
            connection.as_mut().graceful_shutdown();
            if let Ok(Err(error)) =
                tokio::time::timeout(Duration::from_secs(10), &mut connection).await
            {
                log_connection_error(&logger, error);
            }
        }
    }
}

fn log_connection_error(logger: &Arc<JsonLogger>, error: impl std::fmt::Display) {
    let mut event = fields();
    event.insert("error".into(), json!(error.to_string()));
    logger.warn("connection failed", event);
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

pub(crate) async fn drain(mut tasks: JoinSet<()>, grace: Duration) {
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(grace, drain).await.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// One request through the whole pipeline. Always returns a response; errors
/// are represented as HTTP status codes.
pub async fn handle<B>(
    state: Arc<AppState>,
    request: Request<B>,
) -> Result<Response<ReqBody>, Infallible>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let started = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let raw_path = uri.path().to_string();
    let decoded_path = percent_decode_str(&raw_path)
        .decode_utf8_lossy()
        .into_owned();
    let content_length = request.body().size_hint().exact();
    let content_encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = request.into_body();

    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Some(route) = state.keys.select(authorization).cloned() else {
        let request_state = RequestState::new(
            started,
            method.as_str().to_string(),
            decoded_path,
            String::new(),
            None,
            StatusCode::UNAUTHORIZED.as_u16(),
            None,
            None,
            state.logger.clone(),
        );
        request_state.finish();
        return Ok(error_response(StatusCode::UNAUTHORIZED, "Unauthorized\n"));
    };

    let call = state
        .recorder
        .as_ref()
        .and_then(|store| store.begin(method.as_str(), &raw_path, content_length))
        .map(Arc::new);
    let request_body = if let Some(call) = &call {
        CaptureBody {
            inner: body,
            call: call.clone(),
            done: false,
        }
        .boxed_unsync()
    } else {
        body.map_err(|error| Box::new(error) as ForwardError)
            .boxed_unsync()
    };

    // A caller asking for an upgraded tunnel cannot be served by a transparent
    // forwarder; report it with the same fixed category the Go gateway uses.
    if method == Method::CONNECT || headers.contains_key(UPGRADE) {
        let request_state = RequestState::new(
            started,
            method.as_str().to_string(),
            decoded_path,
            String::new(),
            None,
            StatusCode::BAD_GATEWAY.as_u16(),
            call.clone(),
            state.recorder.clone(),
            state.logger.clone(),
        );
        request_state.fail("unsupported_hijack");
        return Ok(finish_local(
            request_state,
            StatusCode::BAD_GATEWAY,
            "Bad Gateway\n",
        ));
    }

    let prepared = match prepare::prepare(
        &route,
        request_body,
        content_length,
        content_encoding.as_deref(),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(PrepareError::UnknownModel(_)) => {
            let request_state = RequestState::new(
                started,
                method.as_str().to_string(),
                decoded_path,
                String::new(),
                None,
                StatusCode::BAD_REQUEST.as_u16(),
                call.clone(),
                state.recorder.clone(),
                state.logger.clone(),
            );
            request_state.fail("preparation_failed");
            return Ok(finish_local(
                request_state,
                StatusCode::BAD_REQUEST,
                "Bad Request\n",
            ));
        }
        Err(PrepareError::BodyTooLarge) => {
            let request_state = RequestState::new(
                started,
                method.as_str().to_string(),
                decoded_path,
                String::new(),
                None,
                StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
                call.clone(),
                state.recorder.clone(),
                state.logger.clone(),
            );
            request_state.fail("preparation_failed");
            return Ok(finish_local(
                request_state,
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request Entity Too Large\n",
            ));
        }
    };

    let is_head = method == Method::HEAD;
    let model = prepared.model.clone();
    let target = prepared.target.clone();
    let request_state = RequestState::new(
        started,
        method.as_str().to_string(),
        decoded_path,
        target.base_url.clone(),
        model,
        StatusCode::BAD_GATEWAY.as_u16(),
        call.clone(),
        state.recorder.clone(),
        state.logger.clone(),
    );

    match state
        .forwarder
        .send(
            method,
            &uri,
            headers,
            prepared.body,
            &target,
            prepared.replace_content_length,
        )
        .await
    {
        Ok(upstream) => {
            let status = upstream.status().as_u16();
            request_state.set_status(status);
            request_state.set_expected_response_bytes(expected_response_bytes(
                is_head,
                status,
                upstream.headers(),
            ));
            let (parts, body) = upstream.into_parts();
            let body = ObservedBody {
                inner: body,
                state: request_state,
                done: false,
            }
            .boxed_unsync();
            Ok(Response::from_parts(parts, body))
        }
        Err(_) => {
            request_state.fail("upstream_transport");
            Ok(finish_local(
                request_state,
                StatusCode::BAD_GATEWAY,
                "Bad Gateway\n",
            ))
        }
    }
}

fn finish_local(state: Arc<RequestState>, status: StatusCode, text: &str) -> Response<ReqBody> {
    state.set_status(status.as_u16());
    if let Some(call) = &state.call {
        call.response.observe(text.as_bytes(), true, false);
    }
    state.finish();
    error_response(status, text)
}

fn expected_response_bytes(is_head: bool, status: u16, headers: &http::HeaderMap) -> Option<u64> {
    if is_head || matches!(status, 204 | 205 | 304) {
        return Some(0);
    }
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn error_response(status: StatusCode, text: &str) -> Response<ReqBody> {
    let body = Full::new(Bytes::copy_from_slice(text.as_bytes()))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        http::header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Per-request log and record finalization, run exactly once.
pub struct RequestState {
    started: Instant,
    method: String,
    path: String,
    target: String,
    model: Option<Model>,
    status: AtomicU16,
    call: Option<Arc<Call>>,
    recorder: Option<Arc<Store>>,
    logger: Arc<JsonLogger>,
    finalized: AtomicBool,
    expected_response_bytes: AtomicU64,
}

impl RequestState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        started: Instant,
        method: String,
        path: String,
        target: String,
        model: Option<Model>,
        status: u16,
        call: Option<Arc<Call>>,
        recorder: Option<Arc<Store>>,
        logger: Arc<JsonLogger>,
    ) -> Arc<Self> {
        Arc::new(Self {
            started,
            method,
            path,
            target,
            model,
            status: AtomicU16::new(status),
            call,
            recorder,
            logger,
            finalized: AtomicBool::new(false),
            expected_response_bytes: AtomicU64::new(u64::MAX),
        })
    }

    fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
    }

    fn set_expected_response_bytes(&self, expected: Option<u64>) {
        self.expected_response_bytes
            .store(expected.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn response_complete_without_eof(&self) -> bool {
        let expected = self.expected_response_bytes.load(Ordering::Relaxed);
        let observed = self
            .call
            .as_ref()
            .map_or(0, |call| call.response.observed_bytes());
        expected != u64::MAX && observed == expected
    }

    fn fail(&self, category: &'static str) {
        if let Some(call) = &self.call {
            call.fail(category);
        }
    }

    fn finish(&self) {
        if self.finalized.swap(true, Ordering::AcqRel) {
            return;
        }
        let status = self.status.load(Ordering::Relaxed);
        if let (Some(call), Some(store)) = (&self.call, &self.recorder) {
            store.finish(call, status, &self.target, self.model.as_ref());
        }
        let mut event = fields();
        event.insert("method".into(), json!(self.method));
        event.insert("path".into(), json!(self.path));
        event.insert("target".into(), json!(self.target));
        event.insert("status".into(), json!(status));
        event.insert(
            "duration".into(),
            json!(self.started.elapsed().as_nanos() as u64),
        );
        if let Some(model) = &self.model {
            event.insert("model".into(), json!({"id": model.id, "name": model.name}));
        }
        if let Some(call) = &self.call {
            event.insert("request_id".into(), json!(call.id()));
        }
        self.logger.info("request", event);
    }
}

/// Observes the request body before forwarding, so model rewriting does not
/// change what the record contains.
struct CaptureBody<B> {
    inner: B,
    call: Arc<Call>,
    done: bool,
}

impl<B> Body for CaptureBody<B>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    type Data = Bytes;
    type Error = ForwardError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.call.request.observe(data, false, false);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.call.request.observe(&[], false, true);
                self.done = true;
                Poll::Ready(Some(Err(Box::new(error) as ForwardError)))
            }
            Poll::Ready(None) => {
                self.call.request.observe(&[], true, false);
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Observes response bytes as hyper writes them and finalizes the request when
/// the stream ends, errors, or is dropped by a disconnected client.
struct ObservedBody {
    inner: Incoming,
    state: Arc<RequestState>,
    done: bool,
}

impl Body for ObservedBody {
    type Data = Bytes;
    type Error = ForwardError;

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(data), Some(call)) = (frame.data_ref(), &self.state.call) {
                    call.response.observe(data, false, false);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.state.fail("upstream_read");
                if let Some(call) = &self.state.call {
                    call.response.observe(&[], false, true);
                }
                self.state.finish();
                self.done = true;
                Poll::Ready(Some(Err(Box::new(error) as ForwardError)))
            }
            Poll::Ready(None) => {
                if let Some(call) = &self.state.call {
                    call.response.observe(&[], true, false);
                }
                self.state.finish();
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Hyper can stop polling once it has written a known Content-Length.
        // That is a complete response, not a client abort.
        if !self.state.response_complete_without_eof() {
            self.state.fail("client_write");
            if let Some(call) = &self.state.call {
                call.response.observe(&[], false, true);
            }
        }
        self.state.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use http_body_util::Full;
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;
    use crate::config::{Entry, Model, Target};
    use crate::keymap::Map;

    fn state(directory: &std::path::Path, entries: Vec<Entry>) -> Arc<AppState> {
        let log = directory.join("gateway.log");
        let logger = JsonLogger::file(&log).unwrap();
        let keys = Map::new(HashMap::from([("dev".to_string(), entries)]));
        let store = Store::new(directory.join("conversations"), 1 << 20, logger.clone());
        Arc::new(AppState {
            keys: Arc::new(keys),
            forwarder: Arc::new(Forwarder::new(HashMap::new()).unwrap()),
            recorder: Some(store),
            logger,
        })
    }

    fn entry(models: &[(&str, &str)]) -> Entry {
        Entry {
            target: Target {
                base_url: "http://127.0.0.1:1".into(),
                ..Target::default()
            },
            models: models
                .iter()
                .map(|(id, name)| Model {
                    id: (*id).into(),
                    name: (*name).into(),
                })
                .collect(),
        }
    }

    fn request(
        method: Method,
        path: &str,
        auth: &str,
        body: &'static [u8],
    ) -> Request<Full<Bytes>> {
        let mut request = Request::new(Full::new(Bytes::from_static(body)));
        *request.method_mut() = method;
        *request.uri_mut() = path.parse().unwrap();
        request
            .headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_str(auth).unwrap());
        request
    }

    async fn log_lines(directory: &std::path::Path) -> Vec<Value> {
        let text = std::fs::read_to_string(directory.join("gateway.log")).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn unauthorized_returns_401_and_logs_without_capture() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);
        let request = request(Method::POST, "/v1/chat", "Bearer unknown", b"{}");
        let response = handle(state, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let lines = log_lines(temp.path()).await;
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line["msg"], "request");
        assert_eq!(line["method"], "POST");
        assert_eq!(line["path"], "/v1/chat");
        assert_eq!(line["status"], 401);
        assert_eq!(line["target"], "");
        assert!(line.get("model").is_none());
        assert!(line.get("request_id").is_none());
        assert!(!temp.path().join("conversations").join("x").exists());
    }

    #[tokio::test]
    async fn unknown_model_returns_400_and_records_preparation_failure() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[("known", "real")])]);
        let request = request(
            Method::POST,
            "/v1/chat",
            "Bearer dev",
            br#"{"model":"unknown"}"#,
        );
        let response = handle(state, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let lines = log_lines(temp.path()).await;
        assert_eq!(lines[0]["status"], 400);
        assert!(lines[0].get("request_id").is_some());
        let mut files = Vec::new();
        for day in std::fs::read_dir(temp.path().join("conversations")).unwrap() {
            for file in std::fs::read_dir(day.unwrap().path()).unwrap() {
                files.push(file.unwrap().path());
            }
        }
        assert_eq!(files.len(), 1);
        let record: Value =
            serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
        assert_eq!(record["status"], 400);
        assert_eq!(record["error"], "preparation_failed");
        assert_eq!(record["response_complete"], true);
        assert_eq!(record["response"]["data"], "Bad Request\n");
    }

    #[tokio::test]
    async fn oversized_model_body_returns_413() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[("known", "real")])]);
        let huge = vec![b'x'; crate::proxy::prepare::MAX_INSPECTED_BODY + 1];
        let mut request = request(Method::POST, "/v1/chat", "Bearer dev", b"");
        *request.body_mut() = Full::new(Bytes::from(huge));
        let response = handle(state, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn upgrade_requests_are_rejected_with_a_fixed_category() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);
        let mut request = request(Method::GET, "/v1/chat", "Bearer dev", b"");
        request
            .headers_mut()
            .insert(UPGRADE, HeaderValue::from_static("websocket"));
        let response = handle(state, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let lines = log_lines(temp.path()).await;
        assert_eq!(lines[0]["status"], 502);
    }
}
