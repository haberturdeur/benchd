//! Talking to the local client daemon.
//!
//! One connection per agent, request/response with unsolicited events mixed in.
//! The daemon holds the session token and substitutes it, so this process never
//! handles one — which means a shim cannot present another agent's.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use benchd_core::wire::{ClaimSpec, ClientMsg, RequestId};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

pub struct Daemon {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    /// Paths arrive *after* a grant, once the device nodes actually exist, so a
    /// claim waits for a second message keyed by lease id.
    paths: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
}

impl Daemon {
    pub async fn connect(socket: &str, name: &str) -> Result<Arc<Self>> {
        let stream = tokio::net::UnixStream::connect(socket).await.with_context(|| {
            format!("failed to connect to the benchd client daemon at {socket}")
        })?;
        let (read, write) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            let mut sink = FramedWrite::new(write, LinesCodec::new());
            while let Some(line) = rx.recv().await {
                if sink.send(line).await.is_err() {
                    break;
                }
            }
        });

        let daemon = Arc::new(Daemon {
            tx,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            paths: Arc::new(Mutex::new(BTreeMap::new())),
            next: AtomicU64::new(1),
        });

        {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                let mut lines = FramedRead::new(read, LinesCodec::new());
                while let Some(Ok(line)) = lines.next().await {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    daemon.dispatch(value).await;
                }
                tracing::warn!("client daemon closed the connection");
            });
        }

        // Registration is a request for a token and always succeeds; there is
        // no authentication (D19).
        daemon.request(ClientMsg::OpenSession { request: RequestId(0), name: name.into() }).await?;
        Ok(daemon)
    }

    async fn dispatch(&self, value: Value) {
        let kind = value.get("msg").and_then(Value::as_str).unwrap_or("");

        // `paths` completes a claim; `failed` aborts one. Both resolve the same
        // waiter, so a claim that cannot be set up returns a reason immediately
        // instead of stalling until the timeout.
        if kind == "paths" || kind == "failed" {
            if let Some(lease) = value.get("lease").and_then(Value::as_u64) {
                if let Some(tx) = self.paths.lock().await.remove(&lease) {
                    let _ = tx.send(value);
                }
            }
            return;
        }

        if let Some(id) = value.get("request").and_then(Value::as_u64) {
            if let Some(tx) = self.pending.lock().await.remove(&id) {
                let _ = tx.send(value);
                return;
            }
        }

        // Unsolicited: revoking, ended, disconnected, or a session_opened from
        // the daemon re-registering us after a coordinator restart. Nothing to
        // correlate; log it so an agent whose device vanished can find out why
        // from the shim's stderr.
        tracing::info!(%value, "event");
    }

    async fn request(&self, mut msg: ClientMsg) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        set_request(&mut msg, RequestId(id));

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.tx
            .send(serde_json::to_string(&msg)?)
            .map_err(|_| anyhow!("the benchd client daemon is not reachable"))?;

        let value = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("the coordinator did not answer in time"))?
            .map_err(|_| anyhow!("the benchd client daemon closed the connection"))?;

        if value.get("msg").and_then(Value::as_str) == Some("error") {
            let detail = value.get("error").and_then(Value::as_str).unwrap_or("unknown error");
            // The retryable flag is the machine-readable form of
            // unsatisfiable-versus-contended; surface it in the text so the
            // agent sees it too (D14).
            let retryable = value.get("retryable").and_then(Value::as_bool).unwrap_or(false);
            return Err(anyhow!(
                "{detail}{}",
                if retryable {
                    "\n\n(the hardware exists but is busy — waiting and retrying will work)"
                } else {
                    "\n\n(this request cannot succeed as written — change it rather than retrying)"
                }
            ));
        }
        Ok(value)
    }

    pub async fn claim(&self, claim: ClaimSpec) -> Result<Value> {
        let granted = self
            .request(ClientMsg::Claim {
                request: RequestId(0),
                session: benchd_core::wire::SessionToken(String::new()),
                claim,
            })
            .await?;

        let lease = granted.get("lease").and_then(Value::as_u64).unwrap_or(0);
        let (tx, rx) = oneshot::channel();
        self.paths.lock().await.insert(lease, tx);

        // A grant is not useful until the device nodes exist; waiting here means
        // the agent's first sight of the lease already includes usable paths.
        let paths = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("the devices were granted but never appeared"))?
            .map_err(|_| anyhow!("the benchd client daemon closed the connection"))?;

        if paths.get("msg").and_then(Value::as_str) == Some("failed") {
            let detail = paths.get("detail").and_then(Value::as_str).unwrap_or("unknown");
            return Err(anyhow!(
                "the claim could not be set up and has been released: {detail}\n\n\
                 (nothing is held; the bench is free for another attempt)"
            ));
        }

        let mut out = granted;
        if let Some(slots) = paths.get("slots") {
            out["slots"] = slots.clone();
        }
        Ok(out)
    }

    pub async fn simple(&self, msg: ClientMsg) -> Result<Value> {
        self.request(msg).await
    }
}

fn set_request(msg: &mut ClientMsg, id: RequestId) {
    use ClientMsg::*;
    match msg {
        OpenSession { request, .. }
        | CloseSession { request, .. }
        | Claim { request, .. }
        | Renew { request, .. }
        | Release { request, .. }
        | Status { request, .. }
        | TagList { request }
        | Done { request, .. } => *request = id,
        Heartbeat => {}
    }
}
