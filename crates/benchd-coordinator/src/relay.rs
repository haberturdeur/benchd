//! The data-plane relay.
//!
//! USB/IP runs the opposite way from our control plane: the machine *with* the
//! device is the one that would normally listen. That is impossible when a host
//! is behind NAT, and it is exactly the reachability D5 refuses. So **both ends
//! dial out** and the coordinator splices the two sockets.
//!
//! Each data channel is its own TCP connection, not a stream multiplexed over
//! the JSON control channel. That means no framing, no channel ids on the wire
//! after the hello, and no backpressure logic — TCP already provides flow
//! control end to end. The coordinator's half is a rendezvous table and
//! `copy_bidirectional`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use benchd_core::wire::{ChannelHello, ChannelKey, ChannelSide};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};

/// How long one half of a channel waits for its partner before giving up.
/// Generous: the client has to be told to materialise, which is a round trip
/// through the coordinator after the host has already dialled.
const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Half-open channels, waiting to be paired.
#[derive(Default)]
pub struct Relay {
    waiting: Mutex<HashMap<ChannelKey, Waiting>>,
}

struct Waiting {
    side: ChannelSide,
    /// Hands the partner's socket to the task that arrived first.
    deliver: oneshot::Sender<TcpStream>,
}

impl Relay {
    /// Take one side of a channel and splice it to the other.
    ///
    /// Whichever side arrives first parks here; the second one hands over its
    /// socket and returns. The waiting task then does the copying, so exactly
    /// one task owns the pump.
    pub async fn join(self: &Arc<Self>, hello: ChannelHello, stream: TcpStream) {
        let key = hello.channel.clone();

        let partner = {
            let mut waiting = self.waiting.lock().await;
            match waiting.remove(&key) {
                Some(other) if other.side != hello.side => Some(other),
                Some(other) => {
                    // Two hosts or two clients for one key: a bug somewhere, and
                    // splicing them would produce a silent stall rather than an
                    // error. Refuse and put the original back.
                    tracing::error!(
                        channel = %key.0, side = ?hello.side,
                        "two identical sides claimed one channel"
                    );
                    waiting.insert(key.clone(), other);
                    return;
                }
                None => None,
            }
        };

        match partner {
            Some(other) => {
                // We are second: hand our socket over and let the first task pump.
                if other.deliver.send(stream).is_err() {
                    tracing::warn!(channel = %key.0, "partner vanished before pairing");
                }
            }
            None => {
                let (tx, rx) = oneshot::channel();
                self.waiting.lock().await.insert(
                    key.clone(),
                    Waiting { side: hello.side, deliver: tx },
                );

                let mut ours = stream;
                match tokio::time::timeout(RENDEZVOUS_TIMEOUT, rx).await {
                    Ok(Ok(mut theirs)) => {
                        tracing::info!(channel = %key.0, "relaying");
                        match tokio::io::copy_bidirectional(&mut ours, &mut theirs).await {
                            Ok((a, b)) => {
                                tracing::info!(channel = %key.0, up = a, down = b, "relay closed")
                            }
                            Err(err) => {
                                tracing::info!(channel = %key.0, ?err, "relay ended")
                            }
                        }
                    }
                    Ok(Err(_)) => tracing::warn!(channel = %key.0, "pairing cancelled"),
                    Err(_) => {
                        self.waiting.lock().await.remove(&key);
                        tracing::warn!(
                            channel = %key.0, side = ?hello.side,
                            "no partner arrived; giving up"
                        );
                    }
                }
            }
        }
    }
}
