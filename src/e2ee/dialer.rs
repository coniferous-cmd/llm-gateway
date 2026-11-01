//! Client-side dialer for encrypted peer gateways.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use tokio::net::TcpStream;

use super::handshake;
use super::identity::{IDENTITY_KEY_SIZE, Identity};
use crate::proxy::forward::{AsyncReadWrite, PeerDialFuture, PeerDialer};

pub struct Dialer {
    identity: Identity,
    /// Peer address (`host:port`) to its pinned identity public key.
    peers: HashMap<String, [u8; IDENTITY_KEY_SIZE]>,
}

impl Dialer {
    pub fn new(identity: Identity, peers: HashMap<String, [u8; IDENTITY_KEY_SIZE]>) -> Self {
        Self { identity, peers }
    }
}

impl PeerDialer for Dialer {
    fn dial(&self, address: String) -> PeerDialFuture {
        let identity = self.identity.clone();
        let pinned = self.peers.get(&address).copied();
        Box::pin(async move {
            let pinned = pinned.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("address {address} is not a configured e2ee peer"),
                )
            })?;
            let stream =
                tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&address))
                    .await
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("dial peer {address}: timed out"),
                        )
                    })?
                    .map_err(|error| {
                        io::Error::new(error.kind(), format!("dial peer {address}: {error}"))
                    })?;
            let _ = stream.set_nodelay(true);
            let session = handshake::initiator(stream, &identity, &pinned)
                .await
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("e2ee handshake with {address}: {error}"),
                    )
                })?;
            Ok(Box::new(session) as Box<dyn AsyncReadWrite>)
        })
    }
}
