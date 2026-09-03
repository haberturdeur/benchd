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

    /// How often to check that this bench's hardware is still attached.
    #[arg(long, default_value_t = 5)]
    device_poll_seconds: u64,
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
    /// Preferred: names the physical port, so swapping the board in it needs no
    /// config change.
    by_path: Option<String>,
    /// Alternative: names one specific chip.
    by_id: Option<String>,
    /// The USB serial expected in this position, if it matters.
    serial: Option<String>,
    busid: Option<String>,
}

/// The USB serial number of the device behind a `/dev/tty*` node.
///
/// Walks up from the tty until it finds the USB device that carries `serial`.
/// The depth is not fixed: a CDC-ACM tty hangs directly off the interface,
/// while a USB-serial bridge adds a `usb-serial` port node in between, so
/// assuming a single parent works for ESP32s and silently fails for CP2102s.
fn usb_serial_of(tty: &std::path::Path) -> Option<String> {
    let name = tty.file_name()?.to_str()?;
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    for _ in 0..6 {
        if let Ok(serial) = std::fs::read_to_string(dir.join("serial")) {
            let serial = serial.trim();
            if !serial.is_empty() {
                return Some(serial.to_string());
            }
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
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
                    let declared = raw
                        .by_path
                        .as_ref()
                        .or(raw.by_id.as_ref())
                        .with_context(|| {
                            format!("resource {name:?} needs by_path (preferred) or by_id")
                        })?;
                    let path = std::path::PathBuf::from(declared);
                    let resolved = std::fs::canonicalize(&path).with_context(|| {
                        format!(
                            "resource {name:?}: {declared} is not present \
                             (board unplugged, or a stale path?)"
                        )
                    })?;

                    // Read the chip's own serial, and check it against the one
                    // declared for this position if there is one. by-path is
                    // stable across a board swap, which is what makes it the
                    // right way to name a bench — and also what makes a swap
                    // silent, so the tags can end up describing hardware that is
                    // no longer there.
                    let observed = usb_serial_of(&resolved);
                    if let Some(want) = raw.serial.as_deref() {
                        match observed.as_deref() {
                            Some(got) if got.eq_ignore_ascii_case(want) => {}
                            Some(got) => anyhow::bail!(
                                "resource {name:?}: expected the board with serial {want} in \
                                 this position, found {got}. Either the board was swapped (update \
                                 this bench's serial and check its tags still describe the \
                                 hardware) or the cable moved."
                            ),
                            None => anyhow::bail!(
                                "resource {name:?}: declares serial {want} but the device's \
                                 serial could not be read"
                            ),
                        }
                    }
                    tracing::info!(
                        %name, path = %resolved.display(),
                        serial = observed.as_deref().unwrap_or("unknown"),
                        "resource present"
                    );
                    Resource::Serial { path, serial: observed }
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

    // Before resolving hardware, release anything a previous incarnation left
    // bound to the USB/IP stub. Resolution goes through the tty, and a
    // stub-bound device has none — so without this a host killed mid-export can
    // never start again, and the board stays dead through every restart.
    {
        let probe = BenchSpec {
            id: config.id.clone(),
            description: String::new(),
            tags: Vec::new(),
            resources: config
                .resources
                .iter()
                .filter_map(|(name, r)| {
                    r.by_id.as_ref().map(|p| {
                        (name.clone(), Resource::Serial { path: std::path::PathBuf::from(p), serial: None })
                    })
                })
                .collect(),
        };
        crate::export::recover_orphans(&probe).await;
    }

    loop {
        // Resolved fresh each time round, so a board that was unplugged and
        // plugged back in is picked up without anyone restarting anything.
        let spec = wait_for_hardware(&config, args.device_poll_seconds).await;
        tracing::info!(bench = %spec.id, resources = spec.resources.len(), "bench");

        // Nothing this process exported can outlive it: the kernel holds its own
        // reference to a handed-over socket, so teardown is mandatory rather
        // than optional. Clear anything a previous incarnation left behind
        // before we accept work (D6).
        let mut exports = Exports::new(spec.clone(), args.coordinator.clone());
        exports.clear_stale().await;

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

/// Block until every resource this bench declares is actually present.
///
/// The alternative — exit and let systemd restart — turns an unplugged board
/// into a restart loop that never ends, and `Restart=always` means it never
/// gives up. Waiting here means a board that is unplugged and plugged back in
/// recovers on its own, and the bench simply is not offered in between.
async fn wait_for_hardware(config: &BenchConfig, poll_seconds: u64) -> BenchSpec {
    let mut complained = false;
    loop {
        match config.to_spec() {
            Ok(spec) => {
                if complained {
                    tracing::info!(bench = %config.id, "hardware is back");
                }
                return spec;
            }
            Err(err) => {
                if !complained {
                    tracing::warn!(
                        bench = %config.id, %err,
                        "waiting for this bench's hardware; it will not be offered until it appears"
                    );
                    complained = true;
                }
                tokio::time::sleep(Duration::from_secs(poll_seconds.max(1))).await;
            }
        }
    }
}

/// Watch this bench's resources and report the first one that disappears.
///
/// Without this, `HostMsg::DeviceLost` was a message nothing ever sent: an
/// unplugged co-located board left the bench registered and matchable forever,
/// because the host kept heartbeating and never touches the device on a
/// co-located export. Every claim then picked the dead bench and failed on the
/// client, and the capability was unusable until someone restarted the host by
/// hand — even with another matching bench free.
async fn watch_hardware(spec: BenchSpec, poll_seconds: u64) -> (String, String) {
    loop {
        tokio::time::sleep(Duration::from_secs(poll_seconds.max(1))).await;
        for (name, resource) in &spec.resources {
            if let Resource::Serial { path, .. } = resource {
                if tokio::fs::metadata(path).await.is_err() {
                    return (
                        name.clone(),
                        format!("{} is no longer present", path.display()),
                    );
                }
            }
        }
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

    let mut watcher = Box::pin(watch_hardware(spec.clone(), args.device_poll_seconds));

    let result = loop {
        tokio::select! {
            lost = &mut watcher => {
                // Tell the coordinator before dropping the connection, so it
                // withdraws the bench and releases its leases rather than
                // waiting out the liveness timeout.
                let (resource, detail) = lost;
                tracing::error!(%resource, %detail, "device lost; withdrawing this bench");
                let msg = HostMsg::DeviceLost { resource, detail };
                if let Ok(line) = serde_json::to_string(&msg) {
                    let _ = sink.send(line).await;
                }
                break Err(anyhow::anyhow!("hardware disappeared"));
            }
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
