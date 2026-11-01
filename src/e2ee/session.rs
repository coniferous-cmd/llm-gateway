//! Async read/write adapter over snow's transport state.
//!
//! Each direction advances its own Noise nonce, so replayed, reordered or
//! spliced frames fail authentication. A frame is a two-byte big-endian length
//! followed by at most one Noise message.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::handshake::{MAX_FRAME_SIZE, MAX_PAYLOAD_PER_FRAME, TAG_SIZE};

pub struct NoiseSession<S> {
    inner: S,
    transport: TransportState,
    read: ReadState,
    write: WriteState,
}

struct ReadState {
    header: [u8; 2],
    header_len: usize,
    cipher: Vec<u8>,
    cipher_len: usize,
    cipher_pos: usize,
    plain: Vec<u8>,
    plain_pos: usize,
    plain_len: usize,
}

struct WriteState {
    pending: Vec<u8>,
    pos: usize,
    plain_len: usize,
}

impl<S> std::fmt::Debug for NoiseSession<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseSession").finish_non_exhaustive()
    }
}

impl<S> NoiseSession<S> {
    pub fn new(inner: S, transport: TransportState, _initiator: bool) -> Self {
        Self {
            inner,
            transport,
            read: ReadState {
                header: [0; 2],
                header_len: 0,
                cipher: Vec::new(),
                cipher_len: 0,
                cipher_pos: 0,
                plain: Vec::new(),
                plain_pos: 0,
                plain_len: 0,
            },
            write: WriteState {
                pending: Vec::new(),
                pos: 0,
                plain_len: 0,
            },
        }
    }
}

impl<S> AsyncRead for NoiseSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.read.plain_pos < this.read.plain_len {
                let available = this.read.plain_len - this.read.plain_pos;
                let take = available.min(buf.remaining());
                buf.put_slice(&this.read.plain[this.read.plain_pos..this.read.plain_pos + take]);
                this.read.plain_pos += take;
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }

            if this.read.header_len < 2 {
                let filled = this.read.header_len;
                let inner = &mut this.inner;
                let header = &mut this.read.header[filled..];
                let mut read_buf = ReadBuf::new(header);
                match Pin::new(inner).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let read = read_buf.filled().len();
                        if read == 0 {
                            if filled == 0 {
                                return Poll::Ready(Ok(())); // clean EOF at a frame boundary
                            }
                            return Poll::Ready(Err(unexpected_eof()));
                        }
                        this.read.header_len += read;
                        continue;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            if this.read.cipher_len == 0 {
                let length = u16::from_be_bytes(this.read.header) as usize;
                if length == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty frame",
                    )));
                }
                this.read.cipher_len = length;
                this.read.cipher_pos = 0;
                this.read.cipher.resize(length, 0);
            }

            if this.read.cipher_pos < this.read.cipher_len {
                let filled = this.read.cipher_pos;
                let inner = &mut this.inner;
                let cipher = &mut this.read.cipher[filled..];
                let mut read_buf = ReadBuf::new(cipher);
                match Pin::new(inner).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let read = read_buf.filled().len();
                        if read == 0 {
                            return Poll::Ready(Err(unexpected_eof()));
                        }
                        this.read.cipher_pos += read;
                        continue;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            this.read.plain.resize(this.read.cipher_len, 0);
            let plain_len = match this
                .transport
                .read_message(&this.read.cipher, &mut this.read.plain)
            {
                Ok(length) => length,
                Err(error) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("decrypt frame: {error}"),
                    )));
                }
            };
            this.read.header_len = 0;
            this.read.cipher_len = 0;
            this.read.cipher_pos = 0;
            this.read.plain_pos = 0;
            this.read.plain_len = plain_len;
        }
    }
}

impl<S> AsyncWrite for NoiseSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.write.pending.is_empty() {
            let chunk = buf.len().min(MAX_PAYLOAD_PER_FRAME);
            let mut sealed = vec![0_u8; chunk + TAG_SIZE];
            let sealed_len = match this.transport.write_message(&buf[..chunk], &mut sealed) {
                Ok(length) => length,
                Err(error) => {
                    return Poll::Ready(Err(io::Error::other(format!("encrypt frame: {error}"))));
                }
            };
            if sealed_len > MAX_FRAME_SIZE {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "encrypted frame exceeds the maximum size",
                )));
            }
            let mut frame = Vec::with_capacity(2 + sealed_len);
            frame.extend_from_slice(&(sealed_len as u16).to_be_bytes());
            frame.extend_from_slice(&sealed[..sealed_len]);
            this.write.pending = frame;
            this.write.pos = 0;
            this.write.plain_len = chunk;
        }
        loop {
            if this.write.pos >= this.write.pending.len() {
                break;
            }
            let inner = &mut this.inner;
            let pending = &this.write.pending[this.write.pos..];
            match Pin::new(inner).poll_write(cx, pending) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write an encrypted frame",
                    )));
                }
                Poll::Ready(Ok(written)) => this.write.pos += written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let accepted = this.write.plain_len.min(buf.len());
        this.write.pending.clear();
        this.write.pos = 0;
        this.write.plain_len = 0;
        Poll::Ready(Ok(accepted))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.write.pos >= this.write.pending.len() {
                break;
            }
            let inner = &mut this.inner;
            let pending = &this.write.pending[this.write.pos..];
            match Pin::new(inner).poll_write(cx, pending) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to flush an encrypted frame",
                    )));
                }
                Poll::Ready(Ok(written)) => this.write.pos += written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        this.write.pending.clear();
        this.write.pos = 0;
        this.write.plain_len = 0;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                let this = self.get_mut();
                Pin::new(&mut this.inner).poll_shutdown(cx)
            }
            other => other,
        }
    }
}

fn unexpected_eof() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "connection closed in the middle of an encrypted frame",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;
    use crate::e2ee::handshake::{initiator, read_frame, responder, write_frame};
    use crate::e2ee::identity::Identity;

    async fn pair() -> (
        NoiseSession<tokio::io::DuplexStream>,
        NoiseSession<tokio::io::DuplexStream>,
    ) {
        let (client, server) = duplex(256 * 1024);
        let client_id = Identity::generate().unwrap();
        let server_id = Identity::generate().unwrap();
        let allow = HashSet::from([client_id.public()]);
        let server_public = server_id.public();
        let (client_session, server_session) = tokio::join!(
            initiator(client, &client_id, &server_public),
            responder(server, &server_id, &allow)
        );
        (client_session.unwrap(), server_session.unwrap())
    }

    #[tokio::test]
    async fn round_trip_in_both_directions_and_large_payloads() {
        let (mut client, mut server) = pair().await;
        let payload = vec![0x5a_u8; 200_000];
        let writer = async {
            client.write_all(&payload).await.unwrap();
            client.flush().await.unwrap();
        };
        let reader = async {
            let mut got = vec![0_u8; payload.len()];
            server.read_exact(&mut got).await.unwrap();
            assert_eq!(got, payload);
        };
        tokio::join!(writer, reader);

        let reply = b"response";
        let writer = async {
            server.write_all(reply).await.unwrap();
            server.flush().await.unwrap();
        };
        let reader = async {
            let mut got = [0_u8; 8];
            client.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, reply);
        };
        tokio::join!(writer, reader);
    }

    #[tokio::test]
    async fn wrong_pinned_key_is_rejected_on_both_sides() {
        let (client, server) = duplex(64 * 1024);
        let client_id = Identity::generate().unwrap();
        let server_id = Identity::generate().unwrap();
        let impostor = Identity::generate().unwrap();
        let allow = HashSet::from([client_id.public()]);
        let impostor_public = impostor.public();
        let (client_result, server_result) = tokio::join!(
            initiator(client, &client_id, &impostor_public),
            responder(server, &server_id, &allow)
        );
        assert!(client_result.is_err());
        assert!(server_result.is_err());
    }

    #[tokio::test]
    async fn unknown_initiator_is_rejected_before_a_reply() {
        let (client, server) = duplex(64 * 1024);
        let client_id = Identity::generate().unwrap();
        let server_id = Identity::generate().unwrap();
        let allow = HashSet::new();
        let server_public = server_id.public();
        let (client_result, server_result) = tokio::join!(
            initiator(client, &client_id, &server_public),
            responder(server, &server_id, &allow)
        );
        assert!(client_result.is_err());
        let error = server_result.unwrap_err();
        assert!(error.to_string().contains("allowlist"), "{error}");
    }

    #[tokio::test]
    async fn framing_rejects_empty_frames() {
        let (mut writer, mut reader) = duplex(64);
        writer.write_all(&0_u16.to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn framing_round_trips_payloads() {
        let (mut writer, mut reader) = duplex(64 * 1024);
        let payload = vec![1_u8; 1000];
        let write = write_frame(&mut writer, &payload);
        let read = read_frame(&mut reader);
        let (write_result, read_result) = tokio::join!(write, read);
        write_result.unwrap();
        assert_eq!(read_result.unwrap(), payload);
    }
}
