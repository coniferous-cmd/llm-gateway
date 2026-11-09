//! Transparent response decoding.
//!
//! The gateway promises the caller plaintext: any single `Content-Encoding` it
//! advertised upstream is decoded here, incrementally, so streaming responses
//! stay streaming. Unsupported or layered codings are rejected before a body
//! byte is forwarded.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use bytes::{Buf, Bytes};
use http::HeaderMap;
use http::header::CONTENT_ENCODING;
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

use super::forward::BoxError;
use super::prepare::ReqBody;

/// A content coding the gateway can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Gzip,
    /// RFC 9110 `deflate`, which is zlib-wrapped DEFLATE.
    Deflate,
    Br,
    Zstd,
}

/// The response carries a coding, or a combination of codings, the gateway
/// cannot decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedEncoding;

impl std::fmt::Display for UnsupportedEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unsupported or layered content coding")
    }
}

impl std::error::Error for UnsupportedEncoding {}

/// Classify the response's `Content-Encoding`.
///
/// Absent or `identity` means the body is already plaintext. Exactly one
/// supported coding selects its decoder; layers and unknown codings are
/// refused rather than guessed at.
pub fn classify(headers: &HeaderMap) -> Result<Option<Coding>, UnsupportedEncoding> {
    let mut tokens: Vec<String> = Vec::new();
    for value in headers.get_all(CONTENT_ENCODING) {
        let Ok(value) = value.to_str() else {
            return Err(UnsupportedEncoding);
        };
        for token in value.split(',') {
            let token = token.trim();
            if !token.is_empty() {
                tokens.push(token.to_ascii_lowercase());
            }
        }
    }
    match tokens.as_slice() {
        [] => Ok(None),
        [token] if token == "identity" => Ok(None),
        [token] => coding_from(token).map(Some).ok_or(UnsupportedEncoding),
        _ => Err(UnsupportedEncoding),
    }
}

fn coding_from(token: &str) -> Option<Coding> {
    match token {
        "gzip" => Some(Coding::Gzip),
        "deflate" => Some(Coding::Deflate),
        "br" => Some(Coding::Br),
        "zstd" => Some(Coding::Zstd),
        _ => None,
    }
}

/// A failure while inflating a content-coded body.
#[derive(Debug)]
pub struct DecodeError {
    message: String,
}

impl DecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecodeError {}

/// Marks an error that came from the upstream body rather than the decoder, so
/// it keeps the `upstream_read` category.
#[derive(Debug)]
struct UpstreamBodyError(BoxError);

impl std::fmt::Display for UpstreamBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for UpstreamBodyError {}

/// Adapts a body to `AsyncBufRead` for the streaming decoders.
///
/// The distinction between "no bytes buffered yet" (`Poll::Pending`) and
/// upstream EOF is what keeps incremental decoding correct: a decoder must not
/// read an empty buffer as end of stream.
pub struct BodyBufReader {
    body: ReqBody,
    buffer: Bytes,
    eof: bool,
    input_seen: Arc<AtomicBool>,
}

impl BodyBufReader {
    fn new(body: ReqBody, input_seen: Arc<AtomicBool>) -> Self {
        Self {
            body,
            buffer: Bytes::new(),
            eof: false,
            input_seen,
        }
    }
}

impl AsyncRead for BodyBufReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let available = match self.as_mut().poll_fill_buf(cx) {
            Poll::Ready(Ok(available)) => available,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        };
        if available.is_empty() {
            return Poll::Ready(Ok(()));
        }
        let count = available.len().min(buf.remaining());
        buf.put_slice(&available[..count]);
        self.consume(count);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for BodyBufReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        while this.buffer.is_empty() && !this.eof {
            match Pin::new(&mut this.body).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => this.eof = true,
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.is_empty() => {}
                    Ok(data) => {
                        this.input_seen.store(true, Ordering::Relaxed);
                        this.buffer = data;
                    }
                    // Trailer frames carry no body bytes; the decoded response
                    // is re-framed, so they are dropped.
                    Err(_trailers) => {}
                },
                Poll::Ready(Some(Err(error))) => {
                    this.eof = true;
                    return Poll::Ready(Err(io::Error::other(UpstreamBodyError(error))));
                }
            }
        }
        Poll::Ready(Ok(this.buffer.as_ref()))
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        self.get_mut().buffer.advance(amount);
    }
}

/// The decoded response body handed to the observation layer.
pub struct ReadBody {
    decoder: Box<dyn AsyncRead + Send + Unpin>,
    input_seen: Arc<AtomicBool>,
    produced: bool,
    done: bool,
}

impl ReadBody {
    pub fn new(body: ReqBody, coding: Coding) -> Self {
        let input_seen = Arc::new(AtomicBool::new(false));
        let reader = BodyBufReader::new(body, input_seen.clone());
        let decoder: Box<dyn AsyncRead + Send + Unpin> = match coding {
            Coding::Gzip => Box::new(GzipDecoder::new(reader)),
            Coding::Deflate => Box::new(ZlibDecoder::new(reader)),
            Coding::Br => Box::new(BrotliDecoder::new(reader)),
            Coding::Zstd => Box::new(ZstdDecoder::new(reader)),
        };
        Self {
            decoder,
            input_seen,
            produced: false,
            done: false,
        }
    }
}

impl Body for ReadBody {
    type Data = Bytes;
    type Error = BoxError;

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        let mut scratch = [0u8; 16 * 1024];
        let count = {
            let mut buf = ReadBuf::new(&mut scratch);
            match Pin::new(&mut self.decoder).poll_read(cx, &mut buf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => buf.filled().len(),
                Poll::Ready(Err(error)) => {
                    self.done = true;
                    let message = error.to_string();
                    // An upstream read failure passes through with its own
                    // identity so the observer keeps `upstream_read`.
                    if let Some(inner) = error.into_inner()
                        && let Ok(wrapped) = inner.downcast::<UpstreamBodyError>()
                    {
                        return Poll::Ready(Some(Err(wrapped.0)));
                    }
                    // A zero-length coded body (for example `Content-Encoding:
                    // gzip` with no bytes) makes decoders report a premature
                    // end. With no input and no output, that is an empty
                    // stream.
                    if !self.produced && !self.input_seen.load(Ordering::Relaxed) {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Err(Box::new(DecodeError::new(message)))));
                }
            }
        };
        if count == 0 {
            self.done = true;
            return Poll::Ready(None);
        }
        self.produced = true;
        let data = Bytes::copy_from_slice(&scratch[..count]);
        Poll::Ready(Some(Ok(Frame::data(data))))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;

    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    use http::HeaderValue;
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    async fn encode(coding: Coding, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        match coding {
            Coding::Gzip => {
                let mut encoder = GzipEncoder::new(&mut out);
                encoder.write_all(plain).await.unwrap();
                encoder.shutdown().await.unwrap();
            }
            Coding::Deflate => {
                let mut encoder = ZlibEncoder::new(&mut out);
                encoder.write_all(plain).await.unwrap();
                encoder.shutdown().await.unwrap();
            }
            Coding::Br => {
                let mut encoder = BrotliEncoder::new(&mut out);
                encoder.write_all(plain).await.unwrap();
                encoder.shutdown().await.unwrap();
            }
            Coding::Zstd => {
                let mut encoder = ZstdEncoder::new(&mut out);
                encoder.write_all(plain).await.unwrap();
                encoder.shutdown().await.unwrap();
            }
        }
        out
    }

    /// A body that yields exactly the frames it was given.
    struct Frames(VecDeque<Result<Frame<Bytes>, BoxError>>);

    impl Body for Frames {
        type Data = Bytes;
        type Error = BoxError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn body_from_frames(frames: Vec<Result<Frame<Bytes>, BoxError>>) -> ReqBody {
        Frames(frames.into()).boxed_unsync()
    }

    fn body_from_chunks(chunks: &[&[u8]]) -> ReqBody {
        body_from_frames(
            chunks
                .iter()
                .map(|chunk| Ok(Frame::data(Bytes::copy_from_slice(chunk))))
                .collect(),
        )
    }

    async fn decode(coding: Coding, body: ReqBody) -> Result<Vec<u8>, BoxError> {
        Ok(ReadBody::new(body, coding)
            .collect()
            .await?
            .to_bytes()
            .to_vec())
    }

    fn payload() -> Vec<u8> {
        let mut plain = Vec::new();
        for index in 0..200 {
            plain.extend_from_slice(
                format!("the quick brown fox {index} jumps over the lazy dog\n").as_bytes(),
            );
        }
        plain.extend_from_slice(&[0, 159, 146, 150, 255, 1, 2, 3]);
        plain
    }

    #[tokio::test]
    async fn round_trips_every_coding_across_awkward_frame_boundaries() {
        let plain = payload();
        let sizes = [1usize, 2, 3, 5, 8, 13, 21];
        for coding in [Coding::Gzip, Coding::Deflate, Coding::Br, Coding::Zstd] {
            let encoded = encode(coding, &plain).await;
            let mut chunks = Vec::new();
            let mut offset = 0;
            let mut index = 0;
            while offset < encoded.len() {
                let end = (offset + sizes[index % sizes.len()]).min(encoded.len());
                chunks.push(&encoded[offset..end]);
                offset = end;
                index += 1;
            }
            let decoded = decode(coding, body_from_chunks(&chunks)).await.unwrap();
            assert_eq!(decoded, plain, "{coding:?} round trip differs");
        }
    }

    #[tokio::test]
    async fn truncated_input_is_a_decode_error() {
        let plain = payload();
        for coding in [Coding::Gzip, Coding::Deflate, Coding::Zstd] {
            let encoded = encode(coding, &plain).await;
            let truncated = &encoded[..encoded.len() - 8];
            let error = decode(coding, body_from_chunks(&[truncated]))
                .await
                .unwrap_err();
            assert!(
                error.is::<DecodeError>(),
                "{coding:?} truncation produced {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn corrupt_input_is_a_decode_error() {
        let plain = payload();
        for coding in [Coding::Gzip, Coding::Deflate, Coding::Zstd] {
            let mut encoded = encode(coding, &plain).await;
            let middle = encoded.len() / 2;
            encoded[middle] ^= 0xff;
            let result = decode(coding, body_from_chunks(&[&encoded])).await;
            assert!(
                result.is_err(),
                "{coding:?} corruption decoded to {} bytes",
                result.unwrap().len()
            );
        }
    }

    #[tokio::test]
    async fn empty_input_is_a_clean_empty_stream() {
        for coding in [Coding::Gzip, Coding::Deflate, Coding::Br, Coding::Zstd] {
            let decoded = decode(coding, body_from_chunks(&[])).await.unwrap();
            assert!(decoded.is_empty(), "{coding:?} decoded a non-empty body");
        }
    }

    #[tokio::test]
    async fn upstream_body_errors_keep_their_identity() {
        let upstream = body_from_frames(vec![Err(
            Box::new(io::Error::other("upstream reset")) as BoxError
        )]);
        let error = decode(Coding::Gzip, upstream).await.unwrap_err();
        assert!(!error.is::<DecodeError>());
        assert!(error.to_string().contains("upstream reset"));
    }

    #[tokio::test]
    async fn trailer_frames_are_skipped() {
        let plain = b"hello, trailer".to_vec();
        let encoded = encode(Coding::Gzip, &plain).await;
        let split = encoded.len() / 2;
        let mut trailers = HeaderMap::new();
        trailers.insert("x-checksum", HeaderValue::from_static("abc"));
        let body = body_from_frames(vec![
            Ok(Frame::data(Bytes::copy_from_slice(&encoded[..split]))),
            Ok(Frame::trailers(trailers)),
            Ok(Frame::data(Bytes::copy_from_slice(&encoded[split..]))),
        ]);
        assert_eq!(decode(Coding::Gzip, body).await.unwrap(), plain);
    }

    fn with_encoding(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(CONTENT_ENCODING, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn classify_handles_absent_identity_and_supported_codings() {
        assert_eq!(classify(&HeaderMap::new()).unwrap(), None);
        assert_eq!(classify(&with_encoding(&["identity"])).unwrap(), None);
        assert_eq!(
            classify(&with_encoding(&["GZip"])).unwrap(),
            Some(Coding::Gzip)
        );
        assert_eq!(
            classify(&with_encoding(&["deflate"])).unwrap(),
            Some(Coding::Deflate)
        );
        assert_eq!(classify(&with_encoding(&["br"])).unwrap(), Some(Coding::Br));
        assert_eq!(
            classify(&with_encoding(&["zstd"])).unwrap(),
            Some(Coding::Zstd)
        );
    }

    #[test]
    fn classify_rejects_layers_and_unknown_codings() {
        assert_eq!(
            classify(&with_encoding(&["gzip", "br"])),
            Err(UnsupportedEncoding)
        );
        assert_eq!(
            classify(&with_encoding(&["gzip, br"])),
            Err(UnsupportedEncoding)
        );
        assert_eq!(
            classify(&with_encoding(&["compress"])),
            Err(UnsupportedEncoding)
        );
        assert_eq!(
            classify(&with_encoding(&["gzip", "identity"])),
            Err(UnsupportedEncoding)
        );
    }

    #[test]
    fn infallible_frame_error_type_is_usable() {
        // Keeps the helper's `Infallible` import meaningful without an
        // `unreachable` branch in the body implementations above.
        fn assert_error<T: std::error::Error>() {}
        assert_error::<Infallible>();
    }
}
