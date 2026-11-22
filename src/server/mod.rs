//! HTTP service pipeline: authentication, preparation, forwarding, logging.

mod flush;

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, UPGRADE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use percent_encoding::percent_decode_str;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use self::flush::{DownstreamFlush, FlushBeforeError, FlushIo};
use crate::config::{Model, Target};
use crate::conversationlog::{Call, Store};
use crate::keymap::{Map, Route};
use crate::logging::{JsonLogger, fields};
use crate::proxy::decode::{self, DecodeError, ReadBody};
use crate::proxy::forward::{BoxError as ForwardError, Forwarder, box_response, forwarded_path};
use crate::proxy::prepare::{self, PrepareError, ReqBody};
use crate::proxy::response_model::{self, Endpoint};

/// Digest of the encoded representation; it no longer describes the decoded
/// body, so it is dropped with `Content-Encoding`.
const CONTENT_MD5: http::header::HeaderName = http::header::HeaderName::from_static("content-md5");

/// Presence of this header selects the Anthropic model-list shape.
const ANTHROPIC_VERSION: http::header::HeaderName =
    http::header::HeaderName::from_static("anthropic-version");

/// The path, after the selected target's rewrite rules, answered locally.
const MODEL_LIST_PATH: &str = "/v1/models";

/// Claude Code's connection warm-up probe, answered locally as an empty 200.
/// Its only purpose is to open the transport, so it carries no payload and
/// no provider needs to implement it.
const WARMUP_PATH: &str = "/api/hello";

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
    let flush = Arc::new(DownstreamFlush::default());
    let response_flush = flush.clone();
    let service = service_fn(move |request| {
        let state = state.clone();
        let flush = response_flush.clone();
        async move {
            let response = handle(state, request).await?;
            let (mut parts, body) = response.into_parts();
            let body = if parts.extensions.remove::<FlushBeforeError>().is_some() {
                flush.wrap_body(body)
            } else {
                body
            };
            Ok::<_, Infallible>(Response::from_parts(parts, body))
        }
    });
    let mut connection = Box::pin(
        hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30))
            .keep_alive(true)
            .serve_connection(
                TokioIo::new(FlushIo {
                    inner: stream,
                    flush,
                }),
                service,
            ),
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
    let bodyless = request.body().is_end_stream() || content_length == Some(0);
    let content_encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = request.into_body();

    // Claude Code warms a connection by probing this path without any
    // credentials, so the probe is answered before the token gate: otherwise
    // it would be rejected as unauthorized and the client would report the
    // gateway as unreachable. The base URL may carry any prefix, and no target
    // has been selected yet, so the path is matched by its trailing segment
    // rather than after rewrite rules. The empty 200 carries no payload and no
    // content type, because the caller only reads the connection.
    if matches!(method, Method::GET | Method::HEAD) && bodyless && is_warmup(&decoded_path) {
        let request_state = RequestState::new(
            started,
            method.as_str().to_string(),
            decoded_path.clone(),
            String::new(),
            None,
            None,
            StatusCode::OK.as_u16(),
            None,
            None,
            state.logger.clone(),
        );
        return Ok(finish_local_empty(request_state, StatusCode::OK));
    }

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

    // The model list belongs to the authenticated key, not to a provider, so
    // it is answered here: every live entry contributes its declared ids and
    // no upstream is contacted. The target is only peeked so the rules used
    // to recognize the path come from the entry the request would have
    // reached, without advancing the round-robin counter.
    if method == Method::GET && bodyless && is_model_list(&uri, &route.peek()) {
        let body = model_list_json(&route, headers.contains_key(ANTHROPIC_VERSION));
        let request_state = RequestState::new(
            started,
            method.as_str().to_string(),
            decoded_path,
            String::new(),
            None,
            None,
            StatusCode::OK.as_u16(),
            call.clone(),
            state.recorder.clone(),
            state.logger.clone(),
        );
        return Ok(finish_local_json(request_state, StatusCode::OK, &body));
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
        Err(PrepareError::UnknownModel(id)) => {
            let request_state = RequestState::new(
                started,
                method.as_str().to_string(),
                decoded_path,
                String::new(),
                None,
                None,
                StatusCode::BAD_REQUEST.as_u16(),
                call.clone(),
                state.recorder.clone(),
                state.logger.clone(),
            );
            // Name the id the caller asked for, or the line says only that
            // something was rejected, with an empty target and no model.
            request_state.set_rejected_model(&id);
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
    let response_model = prepared.resolved_model_id().and_then(|id| {
        Endpoint::from_path(&forwarded_path(&uri, &prepared.target))
            .map(|endpoint| (endpoint, id.to_owned()))
    });
    let model = prepared.model.clone();
    let stream = prepared.stream;
    let target = prepared.target.clone();
    let request_state = RequestState::new(
        started,
        method.as_str().to_string(),
        decoded_path,
        target.base_url.clone(),
        model,
        stream,
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
            let bodyless = is_head || matches!(status, 204 | 205 | 304);
            request_state.set_status(status);
            let (mut parts, body) = upstream.into_parts();
            // Bodyless responses carry no bytes to decode, so their headers
            // pass through even when they name a coding.
            let coding = if bodyless {
                None
            } else {
                match decode::classify(&parts.headers) {
                    Ok(coding) => coding,
                    Err(error) => {
                        let _ = request_state.error_detail.set(error.to_string());
                        request_state.fail("upstream_encoding");
                        return Ok(finish_local(
                            request_state,
                            StatusCode::BAD_GATEWAY,
                            "Bad Gateway\n",
                        ));
                    }
                }
            };
            let body = match coding {
                Some(coding) => {
                    parts.headers.remove(CONTENT_ENCODING);
                    parts.headers.remove(CONTENT_LENGTH);
                    parts.headers.remove(CONTENT_MD5);
                    // The decoded length is unknown; completion comes from EOF.
                    request_state.set_expected_response_bytes(None);
                    if body.is_end_stream() {
                        empty_body()
                    } else {
                        ReadBody::new(box_response(body), coding).boxed_unsync()
                    }
                }
                None => {
                    request_state.set_expected_response_bytes(expected_response_bytes(
                        is_head,
                        status,
                        &parts.headers,
                    ));
                    box_response(body)
                }
            };
            let body = match response_model {
                Some((endpoint, id)) if !bodyless && !body.is_end_stream() => {
                    match response_model::restore(
                        body,
                        endpoint,
                        id,
                        parts.status,
                        &mut parts.headers,
                        coding.is_some(),
                    ) {
                        Ok(body) => {
                            request_state.set_expected_response_bytes(None);
                            parts.extensions.insert(FlushBeforeError);
                            body
                        }
                        Err(body) => body,
                    }
                }
                _ => body,
            };
            let body = ObservedBody {
                inner: body,
                state: request_state,
                done: false,
            }
            .boxed_unsync();
            Ok(Response::from_parts(parts, body))
        }
        Err(error) => {
            let mut detail = error.to_string();
            let mut source = error.source();
            while let Some(cause) = source {
                detail.push_str(": ");
                detail.push_str(&cause.to_string());
                source = cause.source();
            }
            let _ = request_state.error_detail.set(detail);
            request_state.fail("upstream_transport");
            Ok(finish_local(
                request_state,
                StatusCode::BAD_GATEWAY,
                "Bad Gateway\n",
            ))
        }
    }
}

/// Whether the request's path, after the selected target's rewrite rules,
/// names the locally answered model list. The query string is ignored.
fn is_model_list(uri: &http::Uri, target: &Target) -> bool {
    forwarded_path(uri, target).split('?').next() == Some(MODEL_LIST_PATH)
}

/// Whether the request path names Claude Code's connection warm-up probe. The
/// probe is issued under the caller's base URL, so any prefix may precede the
/// fixed trailing segment; a path that merely starts with it does not match.
fn is_warmup(path: &str) -> bool {
    path.ends_with(WARMUP_PATH)
}

/// Serialize the key's declared models in the shape the caller's SDK expects:
/// Anthropic when `anthropic-version` was sent, OpenAI otherwise.
fn model_list_json(route: &Route, anthropic: bool) -> String {
    let models = route.models();
    let entries: Vec<serde_json::Value> = models
        .iter()
        .map(|model| model_entry(model, anthropic))
        .collect();
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    let value = if anthropic {
        json!({
            "data": entries,
            "has_more": false,
            "first_id": ids.first(),
            "last_id": ids.last(),
        })
    } else {
        json!({"object": "list", "data": entries})
    };
    value.to_string()
}

/// One model-list entry, in the shape `anthropic` selects, carrying the
/// configured token limits under the wire names the official schema uses —
/// `max_input_tokens` for the context window and `max_tokens` for the output
/// cap. The configuration spells them `max_input_token` / `max_output_token`;
/// a client reading the official fields is what these names exist for.
///
/// The Anthropic `ModelInfo` defines both fields as "number or null" and
/// always includes them, so an undeclared limit goes out as `null`. OpenAI's
/// `Model` has no token-limit field at all, so there an undeclared limit is
/// omitted rather than inventing a `null` for a name no schema owns; a
/// declared one is still published under the Anthropic spelling, since that
/// is the only convention these fields have.
fn model_entry(model: &Model, anthropic: bool) -> serde_json::Value {
    if anthropic {
        return json!({
            "type": "model",
            "id": model.id,
            "display_name": model.id,
            "created_at": "1970-01-01T00:00:00Z",
            "max_input_tokens": model.max_input_token,
            "max_tokens": model.max_output_token,
        });
    }

    let mut entry = json!({
        "id": model.id,
        "object": "model",
        "created": 0,
        "owned_by": "llm-gateway",
    });
    if let Some(map) = entry.as_object_mut() {
        if let Some(limit) = model.max_input_token {
            map.insert("max_input_tokens".to_string(), json!(limit));
        }
        if let Some(limit) = model.max_output_token {
            map.insert("max_tokens".to_string(), json!(limit));
        }
    }
    entry
}

fn finish_local_json(
    state: Arc<RequestState>,
    status: StatusCode,
    text: &str,
) -> Response<ReqBody> {
    state.set_status(status.as_u16());
    if let Some(call) = &state.call {
        call.response.observe(text.as_bytes(), true, false);
    }
    state.finish();
    json_response(status, text)
}

/// A locally answered response with no payload and no content type.
fn finish_local_empty(state: Arc<RequestState>, status: StatusCode) -> Response<ReqBody> {
    state.set_status(status.as_u16());
    if let Some(call) = &state.call {
        call.response.observe(&[], true, false);
    }
    state.finish();
    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    response
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

fn empty_body() -> ReqBody {
    Full::new(Bytes::new())
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn json_response(status: StatusCode, text: &str) -> Response<ReqBody> {
    let body = Full::new(Bytes::copy_from_slice(text.as_bytes()))
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
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
    stream: Option<bool>,
    status: AtomicU16,
    call: Option<Arc<Call>>,
    recorder: Option<Arc<Store>>,
    logger: Arc<JsonLogger>,
    finalized: AtomicBool,
    error_category: OnceLock<&'static str>,
    error_detail: OnceLock<String>,
    expected_response_bytes: AtomicU64,
    /// A model id the caller asked for that nothing under the token serves.
    /// Kept apart from `model`, which only ever holds a resolved model, so a
    /// rejected id cannot be mistaken for one that was routed.
    rejected_model: OnceLock<String>,
}

impl RequestState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        started: Instant,
        method: String,
        path: String,
        target: String,
        model: Option<Model>,
        stream: Option<bool>,
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
            stream,
            status: AtomicU16::new(status),
            call,
            recorder,
            logger,
            finalized: AtomicBool::new(false),
            error_category: OnceLock::new(),
            error_detail: OnceLock::new(),
            expected_response_bytes: AtomicU64::new(u64::MAX),
            rejected_model: OnceLock::new(),
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

    /// Name the model id that could not be resolved, for the log line. The
    /// first call wins, because the id comes from the body actually inspected.
    fn set_rejected_model(&self, id: &str) {
        let _ = self.rejected_model.set(id.to_string());
    }

    fn fail(&self, category: &'static str) {
        let _ = self.error_category.set(category);
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
        if let Some(stream) = self.stream {
            event.insert("stream".into(), json!(stream));
        }
        if let Some(id) = self.rejected_model.get() {
            event.insert("rejected_model".into(), json!(id));
        }
        if let Some(call) = &self.call {
            event.insert("request_id".into(), json!(call.id()));
        }
        if let Some(category) = self.error_category.get() {
            event.insert("error_category".into(), json!(category));
        } else if status == StatusCode::BAD_GATEWAY.as_u16() {
            event.insert("error_category".into(), json!("upstream_status"));
        }
        if let Some(detail) = self.error_detail.get() {
            event.insert("error".into(), json!(detail));
        }
        if status == StatusCode::BAD_GATEWAY.as_u16() {
            self.logger.error("request", event);
        } else {
            self.logger.info("request", event);
        }
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
    inner: ReqBody,
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
                let category = if error.is::<DecodeError>() {
                    "upstream_decode"
                } else {
                    "upstream_read"
                };
                self.state.fail(category);
                if let Some(call) = &self.state.call {
                    call.response.observe(&[], false, true);
                }
                self.state.finish();
                self.done = true;
                Poll::Ready(Some(Err(error)))
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

    use http_body_util::{BodyExt, Full};
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
                    ..Model::default()
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
        // The rejected id is the only clue to which request was refused.
        assert_eq!(lines[0]["rejected_model"], "unknown");
        assert_eq!(lines[0]["target"], "");
        // It must not appear as a resolved model, and the record is unchanged.
        assert!(lines[0].get("model").is_none());
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
    async fn request_log_reports_the_stream_flag() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[("known", "real")])]);
        let streamed = request(
            Method::POST,
            "/v1/chat",
            "Bearer dev",
            br#"{"model":"known","stream":true}"#,
        );
        let response = handle(state.clone(), streamed).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let plain = request(
            Method::POST,
            "/v1/chat",
            "Bearer dev",
            br#"{"model":"known"}"#,
        );
        let response = handle(state, plain).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let lines = log_lines(temp.path()).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["stream"], true);
        assert_eq!(lines[1]["stream"], false);
    }

    async fn json_body(response: Response<ReqBody>) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn model_list_is_answered_locally_for_the_openai_shape() {
        let temp = tempdir().unwrap();
        let state = state(
            temp.path(),
            vec![
                entry(&[("mimo-flash", "real-flash"), ("mimo-pro", "real-pro")]),
                entry(&[("mimo-max", "real-max")]),
            ],
        );
        let response = handle(state, request(Method::GET, "/v1/models", "Bearer dev", b""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = json_body(response).await;
        assert_eq!(body["object"], "list");
        let ids: Vec<_> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["mimo-flash", "mimo-pro", "mimo-max"]);
        assert_eq!(body["data"][0]["object"], "model");
    }

    #[tokio::test]
    async fn model_list_uses_the_anthropic_shape_when_asked() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[("mimo-flash", "real")])]);
        let mut request = request(Method::GET, "/v1/models", "Bearer dev", b"");
        request
            .headers_mut()
            .insert(ANTHROPIC_VERSION, HeaderValue::from_static("2023-06-01"));
        let response = handle(state, request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["has_more"], false);
        assert_eq!(body["data"][0]["type"], "model");
        assert_eq!(body["data"][0]["id"], "mimo-flash");
        assert_eq!(body["first_id"], "mimo-flash");
        assert_eq!(body["last_id"], "mimo-flash");
    }

    /// One entry whose models declare limits in three combinations: both,
    /// output only, and none.
    fn entry_with_limits() -> Entry {
        let mut entry = entry(&[("full", "real-full")]);
        entry.models.push(Model {
            id: "output-only".into(),
            name: "real-output-only".into(),
            max_output_token: Some(32_000),
            ..Model::default()
        });
        entry.models.push(Model {
            id: "undeclared".into(),
            name: "real-undeclared".into(),
            ..Model::default()
        });
        entry.models[0].max_input_token = Some(1_000_000);
        entry.models[0].max_output_token = Some(128_000);
        entry
    }

    fn model_list(body: &Value) -> Vec<Value> {
        body["data"].as_array().unwrap().clone()
    }

    /// The wire name the configuration's `max_input_token` becomes: the
    /// Anthropic `ModelInfo` field a client written against the official
    /// schema actually reads.
    const INPUT_FIELD: &str = "max_input_tokens";
    /// And `max_output_token` becomes `max_tokens`, the official cap on the
    /// request's own `max_tokens` parameter.
    const OUTPUT_FIELD: &str = "max_tokens";

    /// Whether an entry carries a key at all, as opposed to carrying `null`.
    fn carries(entry: &Value, key: &str) -> bool {
        entry.get(key).is_some()
    }

    #[tokio::test]
    async fn model_list_carries_declared_token_limits() {
        let temp = tempdir().unwrap();
        let models = vec![entry_with_limits()];
        let state = state(temp.path(), models);
        let response = handle(state, request(Method::GET, "/v1/models", "Bearer dev", b""))
            .await
            .unwrap();
        let body = json_body(response).await;
        let data = model_list(&body);
        assert_eq!(data[0][INPUT_FIELD], 1_000_000);
        assert_eq!(data[0][OUTPUT_FIELD], 128_000);
        assert_eq!(data[1][OUTPUT_FIELD], 32_000);
        // OpenAI's Model schema has no limit fields, so an undeclared one is
        // omitted here rather than sent as the null Anthropic's schema owns.
        assert!(!carries(&data[1], INPUT_FIELD));
        assert!(!carries(&data[2], INPUT_FIELD));
        assert!(!carries(&data[2], OUTPUT_FIELD));
        // The configuration spellings never reach the wire.
        assert!(!carries(&data[0], "max_input_token"));
        assert!(!carries(&data[0], "max_output_token"));
    }

    #[tokio::test]
    async fn model_list_carries_limits_in_the_anthropic_shape() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry_with_limits()]);
        let mut request = request(Method::GET, "/v1/models", "Bearer dev", b"");
        request
            .headers_mut()
            .insert(ANTHROPIC_VERSION, HeaderValue::from_static("2023-06-01"));
        let response = handle(state, request).await.unwrap();
        let body = json_body(response).await;
        let data = model_list(&body);
        assert_eq!(data[0]["type"], "model");
        assert_eq!(data[0][INPUT_FIELD], 1_000_000);
        assert_eq!(data[0][OUTPUT_FIELD], 128_000);
        assert_eq!(data[1][INPUT_FIELD], Value::Null);
        assert_eq!(data[1][OUTPUT_FIELD], 32_000);
        // Anthropic's ModelInfo declares both fields as "number or null" and
        // always includes them: undeclared means null, not absent.
        assert!(carries(&data[2], INPUT_FIELD));
        assert!(carries(&data[2], OUTPUT_FIELD));
        assert_eq!(data[2][INPUT_FIELD], Value::Null);
        assert_eq!(data[2][OUTPUT_FIELD], Value::Null);
    }

    #[tokio::test]
    async fn model_list_is_empty_for_a_legacy_token() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);
        let response = handle(state, request(Method::GET, "/v1/models", "Bearer dev", b""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["data"].as_array().unwrap().len(), 0);
        assert!(body.get("first_id").is_none());
    }

    #[tokio::test]
    async fn model_list_is_recognized_after_path_rewriting() {
        use crate::config::RewriteRule;

        let temp = tempdir().unwrap();
        let mut entry = entry(&[("mimo-flash", "real")]);
        entry.target.rewrite = vec![RewriteRule {
            source: "/anthropic".into(),
            destination: String::new(),
        }];
        let state = state(temp.path(), vec![entry]);
        let response = handle(
            state,
            request(Method::GET, "/anthropic/v1/models", "Bearer dev", b""),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["data"][0]["id"], "mimo-flash");
    }

    #[tokio::test]
    async fn other_paths_are_still_forwarded() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[("mimo-flash", "real")])]);
        let response = handle(
            state,
            request(Method::GET, "/v1/models/extra", "Bearer dev", b""),
        )
        .await
        .unwrap();
        // No upstream listens on the configured address.
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn warmup_probe_is_answered_locally_without_credentials() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);

        for (method, path) in [
            (Method::HEAD, "/anthropic/api/hello"),
            (Method::GET, "/anthropic/api/hello"),
            (Method::GET, "/api/hello"),
            (Method::GET, "/api/hello?probe=1"),
        ] {
            let response = handle(state.clone(), request(method, path, "", b""))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(response.headers().get(CONTENT_TYPE).is_none());
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert!(bytes.is_empty(), "{path}");
        }

        let lines = log_lines(temp.path()).await;
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["status"], 200);
        assert_eq!(lines[0]["method"], "HEAD");
        assert_eq!(lines[0]["path"], "/anthropic/api/hello");
        assert_eq!(lines[0]["target"], "");
    }

    #[tokio::test]
    async fn a_request_that_only_looks_like_the_warmup_probe_is_still_gated() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);
        for (method, path, body) in [
            (Method::GET, "/api/hello/extra", &b""[..]),
            // A probe carries no body; a POST to the same path is a real call.
            (Method::POST, "/api/hello", &b"{}"[..]),
        ] {
            let response = handle(state.clone(), request(method, path, "", body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn a_path_merely_containing_the_warmup_probe_is_still_forwarded() {
        let temp = tempdir().unwrap();
        let state = state(temp.path(), vec![entry(&[])]);
        let response = handle(
            state,
            request(Method::GET, "/api/hello/extra", "Bearer dev", b""),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
        assert_eq!(lines[0]["level"], "ERROR");
        assert_eq!(lines[0]["error_category"], "unsupported_hijack");
    }
}
