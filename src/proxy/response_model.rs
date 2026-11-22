//! Restore the request-local model ID in eligible plaintext provider responses.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use http::{HeaderMap, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyExt;

use super::model::{ModelSpan, message_model_value, model_value, splice_model_value};
use super::prepare::{BoxError, ReqBody};

const JSON_LIMIT: usize = 32 << 20;
const SSE_LIMIT: usize = 1 << 20;

#[derive(Clone, Copy)]
pub(crate) enum Endpoint {
    ChatCompletions,
    Messages,
}

impl Endpoint {
    pub(crate) fn from_path(path: &str) -> Option<Self> {
        match path.split('?').next()? {
            "/v1/chat/completions" => Some(Self::ChatCompletions),
            "/v1/messages" => Some(Self::Messages),
            _ => None,
        }
    }
}

enum Format {
    Json,
    Sse(Endpoint),
}

fn format(
    endpoint: Endpoint,
    status: StatusCode,
    headers: &HeaderMap,
    decoded: bool,
) -> Option<Format> {
    if !status.is_success() {
        return None;
    }
    let media_type = headers
        .get(CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .trim();
    if media_type.eq_ignore_ascii_case("text/event-stream") {
        Some(Format::Sse(endpoint))
    } else if media_type.eq_ignore_ascii_case("application/json") {
        let oversized = !decoded
            && headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
                .is_some_and(|length| length > JSON_LIMIT as u64);
        (!oversized).then_some(Format::Json)
    } else {
        None
    }
}

fn remove_body_metadata(headers: &mut HeaderMap) {
    headers.remove(CONTENT_LENGTH);
    headers.remove(ETAG);
    for name in ["content-md5", "digest", "content-digest", "repr-digest"] {
        headers.remove(name);
    }
}

fn validated_model(payload: &[u8], find: fn(&[u8]) -> Option<ModelSpan>) -> Option<ModelSpan> {
    // Validate without building a JSON tree or interpreting unrelated numbers.
    let text = std::str::from_utf8(payload).ok()?;
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    find(payload)
}

fn rewrite_json(payload: Vec<u8>, id: &str) -> Bytes {
    if let Some(span) = validated_model(&payload, model_value)
        && span.value != id
    {
        splice_model_value(&payload, &span, id).into()
    } else {
        payload.into()
    }
}

fn rewrite_event(event: Vec<u8>, endpoint: Endpoint, id: &str) -> Bytes {
    let mut event_name = &[][..];
    let mut data = Vec::new();
    let mut payload = Vec::new();
    let mut line_start = 0;
    while line_start < event.len() {
        let line_end = event[line_start..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
            .map_or(event.len(), |offset| line_start + offset);
        let line = &event[line_start..line_end];
        let field_end = line
            .iter()
            .position(|&byte| byte == b':')
            .unwrap_or(line.len());
        let value_start = (field_end + 1).min(line.len());
        let value_start = value_start + usize::from(line.get(value_start) == Some(&b' '));
        let value = &line[value_start..];
        match &line[..field_end] {
            b"event" => event_name = value,
            b"data" => {
                if !data.is_empty() {
                    payload.push(b'\n');
                }
                let start = payload.len();
                payload.extend_from_slice(value);
                data.push((start..payload.len(), line_start + value_start));
            }
            _ => {}
        }
        line_start = line_end
            + if event.get(line_end..line_end + 2) == Some(b"\r\n") {
                2
            } else {
                1
            };
    }
    let find = match endpoint {
        Endpoint::ChatCompletions if matches!(event_name, b"" | b"message") => model_value,
        Endpoint::Messages if event_name == b"message_start" => message_model_value,
        _ => return event.into(),
    };
    if let Some(mut span) = validated_model(&payload, find)
        && span.value != id
        && let Some((range, offset)) = data
            .iter()
            .find(|(range, _)| range.start <= span.start && span.end <= range.end)
    {
        span.start = offset + (span.start - range.start);
        span.end = offset + (span.end - range.start);
        splice_model_value(&event, &span, id).into()
    } else {
        event.into()
    }
}

/// Wrap a response only when its endpoint, status and media type allow it.
/// Changing an outgoing body length also changes how the observer sees EOF.
pub(crate) fn restore(
    body: ReqBody,
    endpoint: Endpoint,
    id: String,
    status: StatusCode,
    headers: &mut HeaderMap,
    decoded: bool,
) -> Result<ReqBody, ReqBody> {
    let Some(format) = format(endpoint, status, headers, decoded) else {
        return Err(body);
    };
    remove_body_metadata(headers);
    let mode = match format {
        Format::Json => Mode::Json,
        Format::Sse(endpoint) => Mode::Sse {
            endpoint,
            framing: SseFraming::default(),
            oversized: false,
        },
    };
    Ok(RestoreBody {
        inner: body,
        id,
        mode,
        input: Bytes::new(),
        buffer: Vec::new(),
        trailers: None,
        end: None,
    }
    .boxed_unsync())
}

enum Mode {
    Json,
    Sse {
        endpoint: Endpoint,
        framing: SseFraming,
        oversized: bool,
    },
    Passthrough,
}

struct RestoreBody {
    inner: ReqBody,
    id: String,
    mode: Mode,
    /// Unconsumed bytes of the current upstream data frame.
    input: Bytes,
    buffer: Vec<u8>,
    trailers: Option<HeaderMap>,
    end: Option<Result<(), BoxError>>,
}

impl RestoreBody {
    fn process_data(&mut self) -> Option<Bytes> {
        match &mut self.mode {
            Mode::Json if self.input.len() <= JSON_LIMIT - self.buffer.len() => {
                append_bounded(
                    &mut self.buffer,
                    &std::mem::take(&mut self.input),
                    JSON_LIMIT,
                );
                None
            }
            Mode::Json => {
                self.mode = Mode::Passthrough;
                if self.buffer.is_empty() {
                    Some(std::mem::take(&mut self.input))
                } else {
                    Some(std::mem::take(&mut self.buffer).into())
                }
            }
            Mode::Passthrough => Some(std::mem::take(&mut self.input)),
            Mode::Sse {
                endpoint,
                framing,
                oversized,
            } => {
                if std::mem::take(&mut framing.trailing_lf) && self.input[0] == b'\n' {
                    return Some(self.input.split_to(1));
                }
                let limit = if *oversized {
                    self.input.len()
                } else {
                    self.input.len().min(SSE_LIMIT - self.buffer.len())
                };
                let (consumed, complete) = framing.scan(&self.input, limit);
                let bytes = self.input.split_to(consumed);
                if *oversized {
                    *oversized = !complete;
                    return (!bytes.is_empty()).then_some(bytes);
                }

                append_bounded(&mut self.buffer, &bytes, SSE_LIMIT);
                // A trailing CR already ends the blank line. Below the cap,
                // an optional LF cannot change eligibility, so send the event
                // now and relay that LF separately if it arrives.
                let emits_at_cr =
                    self.input.is_empty() && framing.blank_cr && self.buffer.len() < SSE_LIMIT;
                if emits_at_cr {
                    *framing = SseFraming {
                        trailing_lf: true,
                        ..SseFraming::default()
                    };
                }
                if complete || emits_at_cr {
                    Some(rewrite_event(
                        std::mem::take(&mut self.buffer),
                        *endpoint,
                        &self.id,
                    ))
                } else if !self.input.is_empty() {
                    // The first byte over the cap belongs to this event.
                    *oversized = true;
                    Some(std::mem::take(&mut self.buffer).into())
                } else {
                    None
                }
            }
        }
    }

    fn finish_data(&mut self, clean: bool) -> Option<Bytes> {
        if self.buffer.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.buffer);
        Some(match self.mode {
            Mode::Json if clean => rewrite_json(bytes, &self.id),
            // A CR awaiting an optional LF terminates the event at EOF.
            Mode::Sse {
                endpoint, framing, ..
            } if clean && framing.blank_cr => rewrite_event(bytes, endpoint, &self.id),
            _ => bytes.into(),
        })
    }
}

impl Body for RestoreBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        loop {
            if !self.input.is_empty() {
                if let Some(bytes) = self.process_data() {
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                continue;
            }
            if let Some(clean) = self.end.as_ref().map(Result::is_ok) {
                if let Some(bytes) = self.finish_data(clean) {
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                if let Some(Err(error)) = self.end.replace(Ok(())) {
                    self.trailers = None;
                    return Poll::Ready(Some(Err(error)));
                }
                if let Some(trailers) = self.trailers.take() {
                    return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
                }
                return Poll::Ready(None);
            }
            match ready!(Pin::new(&mut self.inner).poll_frame(cx)) {
                Some(Ok(frame)) if matches!(self.mode, Mode::Passthrough) => {
                    return Poll::Ready(Some(Ok(frame)));
                }
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => self.input = data,
                    Err(frame) => {
                        if let Ok(mut trailers) = frame.into_trailers() {
                            remove_body_metadata(&mut trailers);
                            self.trailers.get_or_insert_default().extend(trailers);
                        }
                    }
                },
                Some(Err(error)) => self.end = Some(Err(error)),
                None => self.end = Some(Ok(())),
            }
        }
    }
}

fn append_bounded(buffer: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    let required = buffer.len() + bytes.len();
    if required > buffer.capacity() {
        let capacity = required.max(buffer.capacity().saturating_mul(2)).min(cap);
        buffer.reserve_exact(capacity - buffer.len());
    }
    buffer.extend_from_slice(bytes);
}

/// Tracks whether the current line is empty and whether a preceding CR can
/// still absorb an LF. At the cap an optional LF can change eligibility.
#[derive(Clone, Copy, Default)]
struct SseFraming {
    nonempty_line: bool,
    after_cr: bool,
    blank_cr: bool,
    trailing_lf: bool,
}

impl SseFraming {
    fn scan(&mut self, data: &[u8], limit: usize) -> (usize, bool) {
        let mut consumed = 0;
        while let Some(&byte) = data.get(consumed) {
            if self.blank_cr && byte != b'\n' {
                *self = Self::default();
                return (consumed, true);
            }
            if consumed == limit {
                break;
            }
            consumed += 1;
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    if self.blank_cr {
                        *self = Self::default();
                        return (consumed, true);
                    }
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.after_cr = true;
                    self.blank_cr = !self.nonempty_line;
                    self.nonempty_line = false;
                }
                b'\n' if !self.nonempty_line => return (consumed, true),
                b'\n' => self.nonempty_line = false,
                _ => self.nonempty_line = true,
            }
        }
        (consumed, false)
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use http_body_util::StreamBody;

    use super::*;

    fn sse_body(chunks: Vec<Bytes>, endpoint: Endpoint) -> ReqBody {
        let frames =
            futures_util::stream::iter(chunks.into_iter().map(|chunk| Ok(Frame::data(chunk))));
        let body = StreamBody::new(frames).boxed_unsync();
        let mut headers =
            HeaderMap::from_iter([(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))]);
        restore(
            body,
            endpoint,
            "alias".into(),
            StatusCode::OK,
            &mut headers,
            false,
        )
        .unwrap_or_else(|_| panic!("SSE must be inspected"))
    }

    #[tokio::test]
    async fn sse_cap_includes_the_delimiter_and_resets_within_a_frame() {
        for (offset, rewritten) in [(0, true), (1, false)] {
            let mut event = b"data: {\"model\":\"real\"}".to_vec();
            event.resize(SSE_LIMIT + offset - 4, b' ');
            event.extend_from_slice(b"\r\n\r\n");
            let mut input = event.clone();
            input.extend_from_slice(b"data: {\"model\":\"real\"}\n\n");

            let body = sse_body(vec![Bytes::from(input)], Endpoint::ChatCompletions);
            let actual = body.collect().await.unwrap().to_bytes();
            let mut expected = event;
            if rewritten {
                let start = b"data: {\"model\":\"".len();
                expected.splice(start..start + 4, b"alias".iter().copied());
            }
            expected.extend_from_slice(b"data: {\"model\":\"alias\"}\n\n");
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn split_crlf_counts_toward_cap() {
        let mut event = b"data: {\"model\":\"real\"}".to_vec();
        event.resize(SSE_LIMIT - 3, b' ');
        event.extend_from_slice(b"\r\n\r");
        let tail = b"\ndata: {\"model\":\"real\"}\r\n\r\n";
        let body = sse_body(
            vec![Bytes::from(event.clone()), Bytes::from_static(tail)],
            Endpoint::ChatCompletions,
        );
        let actual = body.collect().await.unwrap().to_bytes();
        event.extend_from_slice(b"\ndata: {\"model\":\"alias\"}\r\n\r\n");
        assert_eq!(actual, event);
    }

    #[tokio::test]
    async fn lone_cr_delimits_before_more_input_is_available() {
        let chunk = Bytes::from_static(b"data: {\"model\":\"real\"}\r\r");
        let frames = futures_util::stream::iter([Ok(Frame::data(chunk))])
            .chain(futures_util::stream::pending());
        use futures_util::StreamExt;
        let mut headers =
            HeaderMap::from_iter([(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))]);
        let mut body = restore(
            StreamBody::new(frames).boxed_unsync(),
            Endpoint::ChatCompletions,
            "alias".into(),
            StatusCode::OK,
            &mut headers,
            false,
        )
        .unwrap_or_else(|_| panic!("SSE must be inspected"));
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.into_data().unwrap(),
            b"data: {\"model\":\"alias\"}\r\r"[..]
        );
    }

    #[tokio::test]
    async fn anthropic_multiline_data_preserves_raw_event_bytes() {
        let first = b": keep\r\nevent: message_start\r\ndata:  { \"model\":\"outer\",\r\n";
        let second = b"data:\t\"mes\\u0073age\":{\"model\":\"real\"}}\r\n\r";
        let third = b"\nevent: message_start\r\ndata: {\"message\":{\"model\":\"real\"}\r\n\r\nevent: ping\r\ndata: {\"message\":{\"model\":\"real\"}}\r\n\r\n";
        let chunks = [first.as_slice(), second.as_slice(), third.as_slice()]
            .into_iter()
            .map(Bytes::from_static)
            .collect();
        let actual = sse_body(chunks, Endpoint::Messages)
            .collect()
            .await
            .unwrap()
            .to_bytes();

        let mut expected = first.to_vec();
        expected.extend_from_slice(b"data:\t\"mes\\u0073age\":{\"model\":\"alias\"}}\r\n\r");
        expected.extend_from_slice(third);
        assert_eq!(actual, expected);
    }
}
