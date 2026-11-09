//! Request-body inspection and target selection.
//!
//! Model routing buffers at most 32 MiB plus one byte. Legacy tokens may pass
//! larger bodies through: the buffered prefix is replayed ahead of the rest of
//! the stream. Every other byte of an inspected body is preserved.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};

use crate::config::{Model, Target};
use crate::keymap::Route;

use super::model::{model_value, splice_model_value, stream_requested};

/// Bounds how much of a request body model routing reads.
pub const MAX_INSPECTED_BODY: usize = 32 << 20;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ReqBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// A model id that no entry of the token serves.
    UnknownModel(String),
    /// A body beyond the inspection limit on a model-enabled token.
    BodyTooLarge,
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownModel(id) => write!(f, "unknown model id: {id:?}"),
            Self::BodyTooLarge => f.write_str("request body too large"),
        }
    }
}

impl std::error::Error for PrepareError {}

/// The forwarding decision for one authenticated request.
pub struct Prepared {
    pub target: Target,
    /// The caller's model id and upstream name, when one was inspected. For a
    /// legacy token both are the caller's id.
    pub model: Option<Model>,
    /// Whether the caller asked for a streamed response, when the body was
    /// inspected as a JSON object.
    pub stream: Option<bool>,
    pub body: ReqBody,
    /// Whether the caller's `Content-Length` header must be removed because
    /// the body was replaced by a buffered copy.
    pub replace_content_length: bool,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("target", &self.target)
            .field("model", &self.model)
            .field("stream", &self.stream)
            .field("replace_content_length", &self.replace_content_length)
            .finish_non_exhaustive()
    }
}

/// Inspect `body` and select the forwarding target.
pub async fn prepare(
    route: &Route,
    body: ReqBody,
    content_length: Option<u64>,
    content_encoding: Option<&str>,
) -> Result<Prepared, PrepareError> {
    let enforces_limit = route.has_models();
    if body.is_end_stream()
        || content_length == Some(0)
        || encoded(content_encoding)
        || (!enforces_limit
            && content_length.is_some_and(|length| length > MAX_INSPECTED_BODY as u64))
    {
        return Ok(Prepared {
            target: route.round_robin(),
            model: None,
            stream: None,
            body,
            replace_content_length: false,
        });
    }

    let (buffered, rest, read_error) = read_prefix(body).await;
    if let Some(error) = read_error {
        let body = replay_body(buffered, rest, Some(error));
        return Ok(Prepared {
            target: route.round_robin(),
            model: None,
            stream: None,
            body,
            replace_content_length: false,
        });
    }
    if buffered.len() > MAX_INSPECTED_BODY {
        if enforces_limit {
            return Err(PrepareError::BodyTooLarge);
        }
        let body = replay_body(buffered, rest, None);
        return Ok(Prepared {
            target: route.round_robin(),
            model: None,
            stream: None,
            body,
            replace_content_length: false,
        });
    }

    let stream = stream_requested(&buffered);
    let inspected = model_value(&buffered);
    let Some(found) = inspected else {
        let body = full_body(buffered);
        return Ok(Prepared {
            target: route.round_robin(),
            model: None,
            stream,
            body,
            replace_content_length: true,
        });
    };

    if !enforces_limit {
        let model = Model {
            id: found.value.clone(),
            name: found.value.clone(),
        };
        let body = full_body(buffered);
        return Ok(Prepared {
            target: route.round_robin(),
            model: Some(model),
            stream,
            body,
            replace_content_length: true,
        });
    }

    let Some((target, model)) = route.resolve(&found.value) else {
        return Err(PrepareError::UnknownModel(found.value));
    };
    let body = if model.id == model.name {
        full_body(buffered)
    } else {
        full_body(splice_model_value(&buffered, &found, &model.name))
    };
    Ok(Prepared {
        target,
        model: Some(model),
        stream,
        body,
        replace_content_length: true,
    })
}

fn encoded(content_encoding: Option<&str>) -> bool {
    content_encoding.is_some_and(|encoding| {
        let encoding = encoding.trim();
        !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity")
    })
}

/// Read at most `MAX_INSPECTED_BODY + 1` bytes, returning the buffered prefix,
/// the unread remainder of the body, and a read error if one occurred.
async fn read_prefix(mut body: ReqBody) -> (Bytes, ReqBody, Option<BoxError>) {
    let mut buffer = Vec::with_capacity(16 * 1024);
    while buffer.len() <= MAX_INSPECTED_BODY {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    buffer.extend_from_slice(&data);
                }
            }
            Some(Err(error)) => {
                return (Bytes::from(buffer), empty_body(), Some(error));
            }
            None => return (Bytes::from(buffer), empty_body(), None),
        }
    }
    (Bytes::from(buffer), body, None)
}

fn full_body(bytes: impl Into<Bytes>) -> ReqBody {
    Full::new(bytes.into())
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed_unsync()
}

fn empty_body() -> ReqBody {
    full_body(Bytes::new())
}

fn replay_body(prefix: Bytes, rest: ReqBody, error: Option<BoxError>) -> ReqBody {
    ReplayBody {
        prefix,
        rest,
        error,
    }
    .boxed_unsync()
}

struct ReplayBody {
    prefix: Bytes,
    rest: ReqBody,
    error: Option<BoxError>,
}

impl Body for ReplayBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        if let Some(error) = self.error.take() {
            return Poll::Ready(Some(Err(error)));
        }
        Pin::new(&mut self.rest).poll_frame(cx)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// Box an incoming body with a uniform error type.
pub fn box_incoming(body: hyper::body::Incoming) -> ReqBody {
    body.map_err(|error| Box::new(error) as BoxError)
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::Value;

    use super::*;
    use crate::config::{Entry, Model, Target};
    use crate::keymap::Map;

    fn target(base: &str) -> Target {
        Target {
            base_url: base.into(),
            ..Target::default()
        }
    }

    fn route(models: &[(&str, &str)], bases: &[&str]) -> Map {
        let entries = bases
            .iter()
            .enumerate()
            .map(|(index, base)| Entry {
                target: target(base),
                models: if index == 0 {
                    models
                        .iter()
                        .map(|(id, name)| Model {
                            id: (*id).into(),
                            name: (*name).into(),
                        })
                        .collect()
                } else {
                    vec![]
                },
            })
            .collect();
        Map::new(HashMap::from([("dev".to_string(), entries)]))
    }

    fn body(bytes: &[u8]) -> ReqBody {
        full_body(Bytes::copy_from_slice(bytes))
    }

    async fn collect(body: ReqBody) -> Vec<u8> {
        body.collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn rewrites_alias_and_keeps_surrounding_bytes() {
        let routes = route(&[("alias", "real")], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let original = br#"{"model":"alias","n":1}"#;
        let prepared = prepare(route, body(original), Some(original.len() as u64), None)
            .await
            .unwrap();
        assert_eq!(prepared.target.base_url, "https://one");
        assert_eq!(prepared.model.unwrap().name, "real");
        assert_eq!(collect(prepared.body).await, br#"{"model":"real","n":1}"#);
        assert!(prepared.replace_content_length);
    }

    #[tokio::test]
    async fn identical_id_and_name_keeps_bytes() {
        let routes = route(&[("same", "same")], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let original = br#"{"model":"same"}"#;
        let prepared = prepare(route, body(original), None, None).await.unwrap();
        assert_eq!(collect(prepared.body).await, original);
    }

    #[tokio::test]
    async fn unknown_model_and_oversized_body_are_errors() {
        let routes = route(&[("known", "real")], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let error = prepare(route, body(br#"{"model":"unknown"}"#), None, None)
            .await
            .unwrap_err();
        assert_eq!(error, PrepareError::UnknownModel("unknown".into()));

        let mut huge = Vec::with_capacity(MAX_INSPECTED_BODY + 1);
        huge.resize(MAX_INSPECTED_BODY + 1, b'x');
        let error = prepare(route, body(&huge), None, None).await.unwrap_err();
        assert_eq!(error, PrepareError::BodyTooLarge);
    }

    #[tokio::test]
    async fn legacy_token_logs_model_without_rewriting() {
        let routes = route(&[], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let original = br#"{"model":"whatever","n":1}"#;
        let prepared = prepare(route, body(original), None, None).await.unwrap();
        let model = prepared.model.unwrap();
        assert_eq!(model.id, "whatever");
        assert_eq!(model.name, "whatever");
        assert_eq!(collect(prepared.body).await, original);
    }

    #[tokio::test]
    async fn no_model_or_compressed_body_falls_back_to_round_robin() {
        let routes = route(&[("known", "real")], &["https://one", "https://two"]);
        let route = routes.select("Bearer dev").unwrap();
        for (index, original) in [&b"{}"[..], b"not json", b"", br#"{"nested":{"model":"x"}}"#]
            .into_iter()
            .enumerate()
        {
            let prepared = prepare(route, body(original), Some(original.len() as u64), None)
                .await
                .unwrap();
            let expected = if index % 2 == 0 {
                "https://one"
            } else {
                "https://two"
            };
            assert_eq!(prepared.target.base_url, expected);
            assert_eq!(collect(prepared.body).await, original);
        }
        let original = br#"{"model":"known"}"#;
        let prepared = prepare(route, body(original), None, Some("gzip"))
            .await
            .unwrap();
        // Five picks have advanced the counter back to the first entry.
        assert_eq!(prepared.target.base_url, "https://one");
        assert_eq!(collect(prepared.body).await, original);
    }

    #[tokio::test]
    async fn legacy_large_body_is_replayed_unchanged() {
        let routes = route(&[], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let mut huge = vec![b'a'; MAX_INSPECTED_BODY + 17];
        huge[0] = b'{';
        let prepared = prepare(route, body(&huge), None, None).await.unwrap();
        assert_eq!(collect(prepared.body).await, huge);
        assert!(!prepared.replace_content_length);
    }

    #[tokio::test]
    async fn stream_flag_is_observed_from_the_body() {
        let routes = route(&[("known", "real")], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let streamed = br#"{"model":"known","stream":true}"#;
        let prepared = prepare(route, body(streamed), Some(streamed.len() as u64), None)
            .await
            .unwrap();
        assert_eq!(prepared.stream, Some(true));

        let plain = br#"{"model":"known"}"#;
        let prepared = prepare(route, body(plain), Some(plain.len() as u64), None)
            .await
            .unwrap();
        assert_eq!(prepared.stream, Some(false));

        // Compressed bodies are never inspected, so the flag is unknown.
        let prepared = prepare(route, body(streamed), None, Some("gzip"))
            .await
            .unwrap();
        assert_eq!(prepared.stream, None);
    }

    #[tokio::test]
    async fn legacy_token_still_logs_the_stream_flag() {
        let routes = route(&[], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let original = br#"{"model":"whatever","stream":true}"#;
        let prepared = prepare(route, body(original), None, None).await.unwrap();
        assert_eq!(prepared.stream, Some(true));
    }

    #[tokio::test]
    async fn model_id_values_are_decoded() {
        let routes = route(&[("m", "real")], &["https://one"]);
        let route = routes.select("Bearer dev").unwrap();
        let original = br#"{"model":"m"}"#;
        let prepared = prepare(route, body(original), None, None).await.unwrap();
        assert_eq!(prepared.model.unwrap().id, "m");
        let value: Value = serde_json::from_slice(&collect(prepared.body).await).unwrap();
        assert_eq!(value["model"], "real");
    }
}
