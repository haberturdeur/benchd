//! Connection handling: one task per peer, plus the framing.
//!
//! Every executor dials in and holds the connection open; instructions travel
//! back down it (D5). That means each connection needs two independent halves —
//! a reader feeding the shared state machine, and a writer fed by a channel —
//! so the coordinator can push a `Revoking` notice without waiting for the peer
//! to say something first.

use anyhow::{Context, Result};
use futures::SinkExt;
use serde::Serialize;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedWrite, LinesCodec};

/// Handle for pushing messages to one connected peer.
///
/// Cloneable and non-blocking: the state machine holds these and must never
/// block on a slow or dead peer while holding its lock.
#[derive(Clone, Debug)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<String>,
}

impl Outbox {
    pub fn send<T: Serialize>(&self, msg: &T) {
        match serde_json::to_string(msg) {
            Ok(line) => {
                // A closed channel means the peer is gone. That is normal, and
                // the reader task will clean up; nothing to do here.
                let _ = self.tx.send(line);
            }
            Err(err) => tracing::error!(?err, "failed to encode outbound message"),
        }
    }
}

/// Spawn the writer half of a connection, returning its [`Outbox`].
pub fn spawn_writer(write: OwnedWriteHalf, peer: String) -> Outbox {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut framed = FramedWrite::new(write, LinesCodec::new());
        while let Some(line) = rx.recv().await {
            if let Err(err) = framed.send(line).await {
                tracing::debug!(%peer, ?err, "write failed; peer is gone");
                break;
            }
        }
    });
    Outbox { tx }
}

/// Bind, logging the address so a misconfigured port is obvious immediately.
pub async fn listen(addr: &str) -> Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "listening");
    Ok(listener)
}
