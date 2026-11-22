//! Let HTTP/1 flush frames already accepted from a body before it aborts.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::oneshot;

use crate::proxy::prepare::{BoxError, ReqBody};

/// Response extension for bodies that must drain data before a terminal error.
#[derive(Clone)]
pub(super) struct FlushBeforeError;

#[derive(Default)]
pub(super) struct DownstreamFlush(Mutex<Option<oneshot::Sender<()>>>);

impl DownstreamFlush {
    pub(super) fn wrap_body(self: &Arc<Self>, inner: ReqBody) -> ReqBody {
        FlushBody {
            inner,
            flush: self.clone(),
            pending_error: None,
        }
        .boxed_unsync()
    }

    fn wait_for_flush(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        *self.0.lock().unwrap() = Some(sender);
        receiver
    }

    fn flushed(&self) {
        let waiter = self.0.lock().unwrap().take();
        if let Some(waiter) = waiter {
            let _ = waiter.send(());
        }
    }
}

struct FlushBody {
    inner: ReqBody,
    flush: Arc<DownstreamFlush>,
    pending_error: Option<(oneshot::Receiver<()>, BoxError)>,
}

impl Body for FlushBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some((flushed, _)) = &mut self.pending_error {
            let _ = ready!(Pin::new(flushed).poll(cx));
            let (_, error) = self.pending_error.take().unwrap();
            return Poll::Ready(Some(Err(error)));
        }
        match ready!(Pin::new(&mut self.inner).poll_frame(cx)) {
            Some(Err(error)) => {
                let mut flushed = self.flush.wait_for_flush();
                // Register the connection's waker, then give Hyper a chance to
                // write and flush the frames it took before polling this error.
                let registered = Pin::new(&mut flushed).poll(cx);
                debug_assert!(registered.is_pending());
                self.pending_error = Some((flushed, error));
                Poll::Pending
            }
            other => Poll::Ready(other),
        }
    }
}

/// Acknowledges a drain only after the underlying stream successfully flushes.
/// Hyper calls this after its HTTP write buffer is empty.
pub(super) struct FlushIo<S> {
    pub(super) inner: S,
    pub(super) flush: Arc<DownstreamFlush>,
}

impl<S: AsyncRead + Unpin> AsyncRead for FlushIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FlushIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bytes)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.inner).poll_flush(cx))?;
        self.flush.flushed();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::error::Error;
    use std::time::Duration;

    use futures_util::StreamExt;
    use http_body_util::StreamBody;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn http1_writes_body_before_returning_error_under_backpressure() {
        let (mut client, server) = duplex(128);
        let (seen_error, received_error) = oneshot::channel();
        let flush = Arc::new(DownstreamFlush::default());
        let payload = Bytes::from(vec![b'x'; 4096]);
        let frames = [
            Ok(Frame::data(payload.clone())),
            Err(Box::new(io::Error::other("original upstream error")) as BoxError),
        ];
        let mut seen_error = Some(seen_error);
        let frames = futures_util::stream::iter(frames).map(move |frame| {
            if frame.is_err() {
                seen_error.take().unwrap().send(()).unwrap();
            }
            frame
        });

        let body = Mutex::new(Some(
            flush.wrap_body(StreamBody::new(frames).boxed_unsync()),
        ));
        let service = service_fn(move |_| {
            std::future::ready(Ok::<_, Infallible>(http::Response::new(
                body.lock().unwrap().take().unwrap(),
            )))
        });
        let server = tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(
            TokioIo::new(FlushIo {
                inner: server,
                flush,
            }),
            service,
        ));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(2), received_error)
            .await
            .unwrap()
            .unwrap();
        // The full body cannot fit in the duplex buffer before the client reads.
        assert!(!server.is_finished());
        let mut wire = Vec::new();
        timeout(Duration::from_secs(2), client.read_to_end(&mut wire))
            .await
            .unwrap()
            .unwrap();
        let header_end = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let mut expected = b"1000\r\n".to_vec();
        expected.extend_from_slice(&payload);
        expected.extend_from_slice(b"\r\n");
        assert_eq!(&wire[header_end..], expected);
        let error = server.await.unwrap().unwrap_err();
        assert_eq!(
            error.source().unwrap().to_string(),
            "original upstream error"
        );
    }
}
