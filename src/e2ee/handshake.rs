//! Noise IK handshake and length-prefixed framing.

use std::collections::HashSet;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::identity::{IDENTITY_KEY_SIZE, Identity};
use super::session::NoiseSession;

/// Binds every handshake to this protocol version.
pub const PROLOGUE: &[u8] = b"llm-gateway-e2ee/1";
pub const MAX_FRAME_SIZE: usize = 65535;
pub const TAG_SIZE: usize = 16;
pub const MAX_PAYLOAD_PER_FRAME: usize = MAX_FRAME_SIZE - TAG_SIZE;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn params() -> snow::params::NoiseParams {
    "Noise_IK_25519_ChaChaPoly_BLAKE2s"
        .parse()
        .expect("built-in Noise parameters are valid")
}

fn noise_error(context: &str, error: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{context}: {error}"))
}

/// Run the initiator side of IK, pinned to `peer_public`.
pub async fn initiator<S>(
    stream: S,
    identity: &Identity,
    peer_public: &[u8; IDENTITY_KEY_SIZE],
) -> io::Result<NoiseSession<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let exchange = async move {
        let private = identity.private();
        let peer = *peer_public;
        let mut handshake = snow::Builder::new(params())
            .local_private_key(&private)
            .map_err(|error| noise_error("initiator handshake key", error))?
            .remote_public_key(&peer)
            .map_err(|error| noise_error("initiator peer key", error))?
            .prologue(PROLOGUE)
            .map_err(|error| noise_error("initiator prologue", error))?
            .build_initiator()
            .map_err(|error| noise_error("initiator handshake state", error))?;

        let mut message = vec![0_u8; MAX_FRAME_SIZE];
        let length = handshake
            .write_message(&[], &mut message)
            .map_err(|error| noise_error("initiator handshake write", error))?;
        let mut stream = stream;
        write_frame(&mut stream, &message[..length]).await?;

        let reply = read_frame(&mut stream).await?;
        let mut payload = vec![0_u8; MAX_FRAME_SIZE];
        handshake
            .read_message(&reply, &mut payload)
            .map_err(|error| {
                noise_error("finish initiator handshake (peer identity rejected)", error)
            })?;
        let transport = handshake
            .into_transport_mode()
            .map_err(|error| noise_error("initiator transport mode", error))?;
        Ok(NoiseSession::new(stream, transport, true))
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "e2ee handshake timed out"))?
}

/// Run the responder side of IK. The initiator's identity is checked against
/// `allow` before any reply is written.
pub async fn responder<S>(
    stream: S,
    identity: &Identity,
    allow: &HashSet<[u8; IDENTITY_KEY_SIZE]>,
) -> io::Result<NoiseSession<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let exchange = async move {
        let private = identity.private();
        let mut handshake = snow::Builder::new(params())
            .local_private_key(&private)
            .map_err(|error| noise_error("responder handshake key", error))?
            .prologue(PROLOGUE)
            .map_err(|error| noise_error("responder prologue", error))?
            .build_responder()
            .map_err(|error| noise_error("responder handshake state", error))?;

        let mut stream = stream;
        let message = read_frame(&mut stream).await?;
        let mut payload = vec![0_u8; MAX_FRAME_SIZE];
        handshake
            .read_message(&message, &mut payload)
            .map_err(|error| noise_error("read initiator message", error))?;

        let peer = handshake.get_remote_static().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "initiator identity missing")
        })?;
        let peer: [u8; IDENTITY_KEY_SIZE] = peer.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "initiator identity must be a 32-byte X25519 key",
            )
        })?;
        if !allow.contains(&peer) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("initiator identity {} not in peer allowlist", hex(&peer)),
            ));
        }

        let mut reply = vec![0_u8; MAX_FRAME_SIZE];
        let length = handshake
            .write_message(&[], &mut reply)
            .map_err(|error| noise_error("responder handshake write", error))?;
        write_frame(&mut stream, &reply[..length]).await?;
        let transport = handshake
            .into_transport_mode()
            .map_err(|error| noise_error("responder transport mode", error))?;
        Ok(NoiseSession::new(stream, transport, false))
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "e2ee handshake timed out"))?
}

pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame of {} bytes exceeds {MAX_FRAME_SIZE}", payload.len()),
        ));
    }
    writer
        .write_all(&(payload.len() as u16).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

pub async fn read_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 2];
    reader.read_exact(&mut header).await?;
    let length = u16::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
