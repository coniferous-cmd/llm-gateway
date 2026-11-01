//! Encrypted peer listener.
//!
//! Every accepted connection must complete the responder handshake with a
//! pinned identity before any HTTP bytes are exchanged. Rejected identities
//! are closed and logged without leaking key material.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::handshake;
use super::identity::{IDENTITY_KEY_SIZE, Identity};
use crate::logging::fields;
use crate::server::{self, AppState};

pub async fn serve<F>(
    listener: TcpListener,
    identity: Identity,
    allow: HashSet<[u8; IDENTITY_KEY_SIZE]>,
    state: Arc<AppState>,
    shutdown: F,
) where
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
                    Ok((stream, address)) => {
                        let identity = identity.clone();
                        let allow = allow.clone();
                        let state = state.clone();
                        let logger = logger.clone();
                        let conn_shutdown_rx = conn_shutdown.subscribe();
                        tasks.spawn(async move {
                            let remote = address.to_string();
                            match handshake::responder(stream, &identity, &allow).await {
                                Ok(session) => {
                                    let mut event = fields();
                                    event.insert("remote".into(), json!(remote));
                                    logger.info("e2ee session established", event);
                                    server::serve_connection(session, state, conn_shutdown_rx).await;
                                }
                                Err(error) => {
                                    let mut event = fields();
                                    event.insert("remote".into(), json!(remote));
                                    event.insert("error".into(), json!(error.to_string()));
                                    logger.warn("e2ee handshake rejected", event);
                                }
                            }
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
    server::drain(tasks, Duration::from_secs(10)).await;
}
