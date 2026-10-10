//! Talking to the local client daemon.
//!
//! One logical connection per agent, request/response with unsolicited events
//! mixed in. The daemon holds the session token and substitutes it, so this
//! process never handles one — which means a shim cannot present another
//! agent's.
//!
//! The unix socket is a link, not the agent's lifetime. Stdio is. When the
//! client daemon restarts the socket vanishes and comes back; this redials,
//! opens a fresh session under the same name, and retries in-flight calls.
//! Leases still die with the daemon (D6). Tools do not.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use benchd_core::wire::{ClaimSpec, ClientMsg, RequestId};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

const FIRST_WAIT: Duration = Duration::from_secs(30);
const CALL_WAIT: Duration = Duration::from_secs(30);
const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(2);

pub struct Daemon {
    socket: String,
    name: String,
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    incompatible: Mutex<Option<String>>,
    /// Keep retrying transient failures, but retain their cause for callers
    /// whose deadline expires before the daemon recovers.
    last_link_error: Mutex<Option<String>>,
    up: Notify,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    /// Paths arrive *after* a grant, once the device nodes actually exist, so a
    /// claim waits for a second message keyed by lease id.
    paths: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
}

impl Daemon {
    pub async fn connect(socket: &str, name: &str) -> Result<Arc<Self>> {
        let daemon = Arc::new(Daemon {
            socket: socket.to_string(),
            name: name.to_string(),
            tx: Mutex::new(None),
            incompatible: Mutex::new(None),
            last_link_error: Mutex::new(None),
            up: Notify::new(),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            paths: Arc::new(Mutex::new(BTreeMap::new())),
            next: AtomicU64::new(1),
        });
        {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move { daemon.pump().await });
        }
        daemon.wait_until_up(FIRST_WAIT).await.with_context(|| {
            format!("failed to connect to the benchd client daemon at {socket}")
        })?;
        Ok(daemon)
    }

    async fn wait_until_up(&self, budget: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let notified = self.up.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.tx.lock().await.is_some() {
                return Ok(());
            }
            if let Some(error) = self.incompatible.lock().await.as_ref() {
                return Err(anyhow!(error.clone()));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(self
                    .connection_error("the benchd client daemon did not come up in time")
                    .await);
            }
        }
    }

    async fn connection_error(&self, fallback: &str) -> anyhow::Error {
        match self.last_link_error.lock().await.as_ref() {
            Some(error) => anyhow!("{fallback}: {error}"),
            None => anyhow!("{fallback}"),
        }
    }

    async fn pump(&self) {
        let mut delay = RETRY_MIN;
        loop {
            match self.try_link().await {
                Ok(()) => {
                    delay = RETRY_MIN;
                    tokio::time::sleep(RETRY_MIN).await;
                }
                Err(err) => {
                    *self.last_link_error.lock().await = Some(format!("{err:#}"));
                    if err
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidData)
                    {
                        *self.incompatible.lock().await = Some(format!("{err:#}"));
                        self.up.notify_waiters();
                    }
                    tracing::warn!(
                        socket = %self.socket,
                        error = format!("{err:#}"),
                        "client daemon unreachable; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
        }
    }

    /// Drive one socket until it dies. `Ok` means it was up and then closed;
    /// `Err` means it never registered.
    async fn try_link(&self) -> Result<()> {
        let mut stream = tokio::net::UnixStream::connect(&self.socket).await?;
        benchd_core::protocol::connect(&mut stream).await?;
        let (read, write) = stream.into_split();
        let mut sink = FramedWrite::new(write, LinesCodec::new());
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if sink.send(line).await.is_err() {
                    break;
                }
            }
        });

        let (dead_tx, dead_rx) = oneshot::channel();
        {
            let mut lines = FramedRead::new(read, LinesCodec::new());
            let pending = Arc::clone(&self.pending);
            let paths = Arc::clone(&self.paths);
            tokio::spawn(async move {
                while let Some(Ok(line)) = lines.next().await {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    dispatch(&pending, &paths, value).await;
                }
                let _ = dead_tx.send(());
            });
        }

        self.call_on(
            &tx,
            ClientMsg::OpenSession {
                request: RequestId(0),
                name: self.name.clone(),
            },
        )
        .await?;
        *self.incompatible.lock().await = None;
        *self.last_link_error.lock().await = None;
        *self.tx.lock().await = Some(tx);
        self.up.notify_waiters();
        tracing::info!(socket = %self.socket, "connected to the client daemon");

        let _ = dead_rx.await;
        *self.tx.lock().await = None;
        self.fail_inflight().await;
        tracing::warn!("client daemon closed the connection; reconnecting");
        Ok(())
    }

    async fn fail_inflight(&self) {
        self.pending.lock().await.clear();
        self.paths.lock().await.clear();
    }

    async fn call_on(
        &self,
        tx: &mpsc::UnboundedSender<String>,
        mut msg: ClientMsg,
    ) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        set_request(&mut msg, RequestId(id));

        let (otx, orx) = oneshot::channel();
        self.pending.lock().await.insert(id, otx);
        tx.send(serde_json::to_string(&msg)?)
            .map_err(|_| anyhow!("the benchd client daemon is not reachable"))?;

        let value = tokio::time::timeout(CALL_WAIT, orx)
            .await
            .map_err(|_| anyhow!("the coordinator did not answer in time"))?
            .map_err(|_| anyhow!("the benchd client daemon closed the connection"))?;
        decode_reply(value)
    }

    async fn request(&self, msg: ClientMsg) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + CALL_WAIT;
        loop {
            let tx = loop {
                let notified = self.up.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(tx) = self.tx.lock().await.clone() {
                    break tx;
                }
                if let Some(error) = self.incompatible.lock().await.as_ref() {
                    return Err(anyhow!(error.clone()));
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    return Err(self
                        .connection_error("the benchd client daemon is not reachable")
                        .await);
                }
            };
            match self.call_on(&tx, msg.clone()).await {
                Ok(value) => return Ok(value),
                Err(err) if is_link_error(&err) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(err);
                    }
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn claim(&self, claim: ClaimSpec) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + CALL_WAIT + CALL_WAIT;
        loop {
            let granted = self
                .request(ClientMsg::Claim {
                    request: RequestId(0),
                    session: benchd_core::wire::SessionToken(String::new()),
                    claim: claim.clone(),
                })
                .await?;

            let lease = granted.get("lease").and_then(Value::as_u64).unwrap_or(0);
            let (tx, rx) = oneshot::channel();
            self.paths.lock().await.insert(lease, tx);

            // A grant is not useful until the device nodes exist; waiting here
            // means the agent's first sight of the lease already includes
            // usable paths.
            match tokio::time::timeout_at(deadline, rx).await {
                Ok(Ok(paths)) => {
                    if paths.get("msg").and_then(Value::as_str) == Some("failed") {
                        let detail = paths
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        return Err(anyhow!(
                            "the claim could not be set up and has been released: {detail}\n\n\
                             (nothing is held; the bench is free for another attempt)"
                        ));
                    }
                    let mut out = granted;
                    if let Some(slots) = paths.get("slots") {
                        out["slots"] = slots.clone();
                    }
                    return Ok(out);
                }
                Ok(Err(_)) if tokio::time::Instant::now() < deadline => continue,
                Ok(Err(_)) | Err(_) => {
                    return Err(anyhow!("the devices were granted but never appeared"));
                }
            }
        }
    }

    pub async fn simple(&self, msg: ClientMsg) -> Result<Value> {
        self.request(msg).await
    }
}

async fn dispatch(
    pending: &Mutex<BTreeMap<u64, oneshot::Sender<Value>>>,
    paths: &Mutex<BTreeMap<u64, oneshot::Sender<Value>>>,
    value: Value,
) {
    let kind = value.get("msg").and_then(Value::as_str).unwrap_or("");

    // `paths` completes a claim; `failed` aborts one. Both resolve the same
    // waiter, so a claim that cannot be set up returns a reason immediately
    // instead of stalling until the timeout.
    if kind == "paths" || kind == "failed" {
        if let Some(lease) = value.get("lease").and_then(Value::as_u64) {
            if let Some(tx) = paths.lock().await.remove(&lease) {
                let _ = tx.send(value);
            }
        }
        return;
    }

    if let Some(id) = value.get("request").and_then(Value::as_u64) {
        if let Some(tx) = pending.lock().await.remove(&id) {
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

fn is_link_error(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("not reachable") || text.contains("closed the connection")
}

fn decode_reply(value: Value) -> Result<Value> {
    if value.get("msg").and_then(Value::as_str) == Some("error") {
        let detail = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        // The retryable flag is the machine-readable form of
        // unsatisfiable-versus-contended; surface it in the text so the
        // agent sees it too (D14).
        let retryable = value
            .get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
        | Inspect { request }
        | PrepareOwner { request, .. }
        | Done { request, .. } => *request = id,
        Heartbeat => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchd_core::lease::SessionId;
    use benchd_core::wire::{SessionToken, TagInfo, ToClient};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn protocol_mismatch_reaches_callers_instead_of_a_generic_timeout() {
        let daemon = Daemon {
            socket: String::new(),
            name: "test".into(),
            tx: Mutex::new(None),
            incompatible: Mutex::new(Some("benchd protocol mismatch: peer old-build".into())),
            last_link_error: Mutex::new(None),
            up: Notify::new(),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            paths: Arc::new(Mutex::new(BTreeMap::new())),
            next: AtomicU64::new(1),
        };
        let error = tokio::time::timeout(Duration::from_secs(1), daemon.wait_until_up(FIRST_WAIT))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("peer old-build"));
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            daemon.simple(ClientMsg::TagList {
                request: RequestId(0),
            }),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("peer old-build"));
    }

    fn socket_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "benchd-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    async fn speak(mut stream: tokio::net::UnixStream) {
        benchd_core::protocol::accept(&mut stream).await.unwrap();
        let (read, write) = stream.into_split();
        let mut lines = FramedRead::new(read, LinesCodec::new());
        let mut sink = FramedWrite::new(write, LinesCodec::new());
        while let Some(Ok(line)) = lines.next().await {
            let Ok(msg) = serde_json::from_str::<ClientMsg>(&line) else {
                continue;
            };
            let reply = match msg {
                ClientMsg::OpenSession { request, .. } => ToClient::SessionOpened {
                    request,
                    session: SessionToken("s".into()),
                    id: SessionId(1),
                },
                ClientMsg::TagList { request } => ToClient::Tags {
                    request,
                    tags: vec![TagInfo {
                        tag: "soc=esp32s3".into(),
                        benches: 1,
                        free: 1,
                        description: String::new(),
                    }],
                },
                other => ToClient::Ok {
                    request: match other {
                        ClientMsg::Heartbeat => RequestId(0),
                        ClientMsg::OpenSession { request, .. }
                        | ClientMsg::CloseSession { request, .. }
                        | ClientMsg::Claim { request, .. }
                        | ClientMsg::Renew { request, .. }
                        | ClientMsg::Release { request, .. }
                        | ClientMsg::Status { request, .. }
                        | ClientMsg::TagList { request }
                        | ClientMsg::Inspect { request }
                        | ClientMsg::PrepareOwner { request, .. }
                        | ClientMsg::Done { request, .. } => request,
                    },
                },
            };
            let Ok(line) = serde_json::to_string(&reply) else {
                continue;
            };
            if sink.send(line).await.is_err() {
                break;
            }
        }
    }

    async fn listen(path: &std::path::Path) -> UnixListener {
        let _ = std::fs::remove_file(path);
        UnixListener::bind(path).unwrap()
    }

    #[tokio::test]
    async fn legacy_handshake_details_reach_waiters_and_calls_and_recovery_clears_them() {
        let path = socket_path();
        let listener = listen(&path).await;
        let server = tokio::spawn(async move {
            // Old daemons ignore unknown messages, including the new hello.
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut lines = FramedRead::new(stream, LinesCodec::new());
                while let Some(Ok(_)) = lines.next().await {}
            }
        });
        let daemon = Arc::new(Daemon {
            socket: path.to_str().unwrap().into(),
            name: "legacy-test".into(),
            tx: Mutex::new(None),
            incompatible: Mutex::new(None),
            last_link_error: Mutex::new(None),
            up: Notify::new(),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            paths: Arc::new(Mutex::new(BTreeMap::new())),
            next: AtomicU64::new(1),
        });
        let pump = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move { daemon.pump().await })
        };
        let (startup, call) = tokio::join!(
            daemon.wait_until_up(Duration::from_secs(6)),
            daemon.simple(ClientMsg::TagList {
                request: RequestId(0)
            }),
        );
        server.abort();
        let _ = server.await;

        // An upgraded daemon must still be reachable after a timed-out hello.
        let listener = listen(&path).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            speak(stream).await;
        });
        let recovered = daemon.wait_until_up(Duration::from_secs(8)).await;
        server.abort();
        let _ = server.await;
        std::fs::remove_file(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while daemon.tx.lock().await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after_recovery = daemon.wait_until_up(Duration::from_millis(100)).await;
        pump.abort();
        let _ = pump.await;

        for error in [startup.unwrap_err(), call.unwrap_err()] {
            let error = error.to_string();
            assert!(error.contains("protocol handshake timed out"), "{error}");
            assert!(error.contains("compatible benchd"), "{error}");
        }
        recovered.unwrap();
        assert!(!after_recovery
            .unwrap_err()
            .to_string()
            .contains("protocol handshake"));
    }

    #[tokio::test]
    async fn a_transient_handshake_timeout_does_not_prevent_connecting() {
        let path = socket_path();
        let listener = listen(&path).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = FramedRead::new(stream, LinesCodec::new());
            while let Some(Ok(_)) = lines.next().await {}
            let (stream, _) = listener.accept().await.unwrap();
            speak(stream).await;
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Daemon::connect(path.to_str().unwrap(), "transient-timeout"),
        )
        .await;
        server.abort();
        let _ = server.await;
        std::fs::remove_file(&path).unwrap();
        assert!(result
            .expect("connection should recover before its deadline")
            .is_ok());
    }

    #[tokio::test]
    async fn a_shim_redials_after_the_client_daemon_restarts() {
        let path = socket_path();
        let listener = listen(&path).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            speak(stream).await;
        });

        let daemon = Daemon::connect(path.to_str().unwrap(), "cursor")
            .await
            .unwrap();
        let first = daemon
            .simple(ClientMsg::TagList {
                request: RequestId(0),
            })
            .await
            .unwrap();
        assert_eq!(first["tags"][0]["tag"], "soc=esp32s3");

        server.abort();
        let _ = server.await;

        let listener = listen(&path).await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            speak(stream).await;
        });

        let second = tokio::time::timeout(
            Duration::from_secs(3),
            daemon.simple(ClientMsg::TagList {
                request: RequestId(0),
            }),
        )
        .await
        .expect("the shim should have redialled")
        .unwrap();
        assert_eq!(second["tags"][0]["tag"], "soc=esp32s3");

        server.abort();
        let _ = std::fs::remove_file(&path);
    }
}
