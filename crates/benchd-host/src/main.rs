//! benchd host: owns the hardware of exactly one bench.
//!
//! Deployed as `benchd-host@<bench>.service`, so N benches is a config concern
//! rather than an architectural one. The blast radius of a wedged host is one
//! bench, and device ownership is never ambiguous.
//!
//! This process **decides nothing**. It dials the coordinator, declares its
//! bench, and executes epoch-qualified instructions. All policy, matching and
//! lease state live at the other end (D6).

mod export;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use benchd_core::model::Resource;
use benchd_core::wire::{BenchSpec, HostMsg, Outcome, RequestId, ToHost, DEFAULT_PORT};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::export::Exports;

#[derive(Parser)]
#[command(name = "benchd-host", about = "benchd host daemon (one per bench)")]
struct Args {
    /// This bench's definition. Lives next to the hardware it describes (D9),
    /// so adding a board is a single-machine operation.
    #[arg(long)]
    config: String,

    /// Coordinator to dial. Only the coordinator listens (D5).
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"))]
    coordinator: String,

    /// Heartbeat interval.
    #[arg(long, default_value_t = 10)]
    heartbeat_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BenchConfig {
    id: String,
    #[serde(default)]
    description: String,
    tags: Vec<String>,
    resources: BTreeMap<String, RawResource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResource {
    #[serde(default = "serial")]
    kind: String,
    by_id: Option<String>,
    busid: Option<String>,
}

fn serial() -> String {
    "serial".into()
}

impl BenchConfig {
    /// Build the registration payload, resolving every resource *now*.
    ///
    /// A bench whose hardware is missing must not register: an unallocatable
    /// bench is strictly better than one that fails at claim time, after an
    /// agent has already been told it has hardware.
    fn to_spec(&self) -> Result<BenchSpec> {
        let tags = self
            .tags
            .iter()
            .map(|t| benchd_core::tags::Tag::parse(t))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid tag in bench config")?;

        let mut resources = BTreeMap::new();
        for (name, raw) in &self.resources {
            let resource = match raw.kind.as_str() {
                "serial" => {
                    let by_id = raw
                        .by_id
                        .as_ref()
                        .with_context(|| format!("resource {name:?} needs by_id"))?;
                    let path = std::path::PathBuf::from(by_id);
                    let resolved = std::fs::canonicalize(&path).with_context(|| {
                        format!(
                            "resource {name:?}: {by_id} is not present \
                             (board unplugged, or a stale by-id path?)"
                        )
                    })?;
                    tracing::info!(%name, path = %resolved.display(), "resource present");
                    Resource::Serial { by_id: path }
                }
                "usb" => {
                    let busid = raw
                        .busid
                        .as_ref()
                        .with_context(|| format!("resource {name:?} needs busid"))?;
                    Resource::Usb { busid: busid.clone() }
                }
                other => anyhow::bail!("resource {name:?} has unknown kind {other:?}"),
            };
            resources.insert(name.clone(), resource);
        }

        Ok(BenchSpec {
            id: self.id.clone(),
            description: self.description.clone(),
            tags,
            resources,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "benchd_host=info".into()),
        )
        .init();

    let args = Args::parse();
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("failed to read {}", args.config))?;
    let config: BenchConfig =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", args.config))?;
    let spec = config.to_spec()?;

    tracing::info!(bench = %spec.id, resources = spec.resources.len(), "bench");

    // Nothing this process exported can outlive it: the kernel holds its own
    // reference to a handed-over socket, so teardown is mandatory rather than
    // optional. Clear anything a previous incarnation left behind before we
    // accept work (D6).
    let mut exports = Exports::new(spec.clone(), args.coordinator.clone());
    exports.clear_stale().await;

    loop {
        match run(&args, &spec, &mut exports).await {
            Ok(()) => tracing::warn!("coordinator closed the connection"),
            Err(err) => tracing::warn!(?err, "connection failed"),
        }
        // The coordinator holds all lease state, so anything we were exporting
        // is void the moment we lose it (D6).
        exports.release_all().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        tracing::info!("reconnecting");
    }
}

async fn run(args: &Args, spec: &BenchSpec, exports: &mut Exports) -> Result<()> {
    let socket = tokio::net::TcpStream::connect(&args.coordinator)
        .await
        .with_context(|| format!("failed to dial {}", args.coordinator))?;
    socket.set_nodelay(true).ok();
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    sink.send(serde_json::to_string(&HostMsg::Register { bench: spec.clone() })?).await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let heartbeat = {
        let tx = tx.clone();
        let period = Duration::from_secs(args.heartbeat_seconds.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Ok(line) = serde_json::to_string(&HostMsg::Heartbeat) else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    };

    let result = loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(line) = outbound else { break Ok(()) };
                sink.send(line).await?;
            }
            inbound = lines.next() => {
                let Some(line) = inbound else { break Ok(()) };
                let line = line?;
                let msg: ToHost = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!(?err, %line, "undecodable message");
                        continue;
                    }
                };
                match msg {
                    ToHost::Registered => tracing::info!("registered"),
                    ToHost::Rejected { reason } => {
                        // Terminal: retrying will not help, and a bench that
                        // matches nothing is worse than no bench at all.
                        tracing::error!(%reason, "registration refused");
                        break Err(anyhow::anyhow!("registration refused: {reason}"));
                    }
                    ToHost::Export { request, lease, epoch, session, channels } => {
                        let result = exports.export(lease, epoch, session, channels).await;
                        reply(&tx, request, result);
                    }
                    ToHost::Unexport { request, lease, epoch } => {
                        let result = exports.unexport(lease, epoch).await;
                        reply(&tx, request, result);
                    }
                }
            }
        }
    };

    heartbeat.abort();
    result
}

fn reply(tx: &tokio::sync::mpsc::UnboundedSender<String>, request: RequestId, result: Outcome) {
    if let Ok(line) = serde_json::to_string(&HostMsg::Done { request, result }) {
        let _ = tx.send(line);
    }
}
