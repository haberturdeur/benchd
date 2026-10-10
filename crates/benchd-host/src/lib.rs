//! benchd host: owns the hardware of exactly one bench.
//!
//! Deployed as `benchd-host@<bench>.service`, so N benches is a config concern
//! rather than an architectural one. The blast radius of a wedged host is one
//! bench, and device ownership is never ambiguous.
//!
//! This process **decides nothing**. It dials the coordinator, declares its
//! bench, and executes epoch-qualified instructions. All policy, matching and
//! lease state live at the other end (D6).
//!
//! It does own one thing outright: its bench's devices are hidden — bound to
//! the USB/IP stub, with no tty — from startup to shutdown, so that hardware
//! this host is responsible for cannot be reached on this machine except
//! through a lease. See [`hide`].

mod export;
mod hide;
pub mod update;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use benchd_core::model::{Resource, UsbNode};
use benchd_core::wire::{BenchSpec, HostMsg, Outcome, RequestId, ToHost, DEFAULT_PORT};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::export::Exports;
use crate::hide::Hidden;

#[derive(Parser)]
pub struct HostArgs {
    /// This bench's definition. Lives next to the hardware it describes (D9),
    /// so adding a board is a single-machine operation.
    #[arg(long, required_unless_present = "release_all")]
    config: Option<String>,

    /// Which bench to give devices back to, for `--release-all`.
    ///
    /// The systemd instance name, which is the bench id. Passing it means the
    /// recovery path never has to read a config file — decommissioning a bench
    /// deletes the config and then stops the unit, and a release that needed
    /// the config would leave those boards stub-bound with no tty.
    #[arg(long)]
    bench: Option<String>,

    /// Coordinator to dial. Only the coordinator listens (D5).
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"))]
    coordinator: String,

    /// Heartbeat interval.
    #[arg(long, default_value_t = 10)]
    heartbeat_seconds: u64,

    /// How often to check that this bench's hardware is still attached.
    #[arg(long, default_value_t = 5)]
    device_poll_seconds: u64,

    /// Where to record which devices this bench has hidden, so a host that is
    /// killed can still give them back on the next start.
    #[arg(long, default_value = "/run/benchd-host")]
    state_dir: String,

    /// Give back every device this bench hid, then exit.
    ///
    /// For `ExecStopPost=`, which systemd runs even when the main process was
    /// killed outright and never reached its own shutdown path. Needs only the
    /// bench id: see `--bench`.
    #[arg(long)]
    release_all: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BenchConfig {
    id: String,
    #[serde(default)]
    description: String,
    /// Markdown given to whoever holds this bench: pinout, jumpers, what is
    /// wired to what. It lives here rather than centrally for the same reason
    /// the tags do (D9) — this file is next to the hardware it describes, so a
    /// rewiring and its documentation are one edit.
    #[serde(default)]
    docs: String,
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
fn usb_serial_of(tty: &std::path::Path) -> Option<String> {
    let dir = benchd_core::sysfs::usb_device_of_tty(tty)?;
    let serial = std::fs::read_to_string(dir.join("serial")).ok()?;
    let serial = serial.trim();
    (!serial.is_empty()).then(|| serial.to_string())
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
        let mut tags = self
            .tags
            .iter()
            .map(|t| benchd_core::tags::Tag::parse(t))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid tag in bench config")?;
        if !tags.iter().any(|tag| tag.key == "host") {
            let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
                .context("cannot read hostname; declare host=<name> in bench tags")?;
            tags.push(
                benchd_core::tags::Tag::parse(&format!(
                    "host={}",
                    hostname.trim().to_ascii_lowercase()
                ))
                .context("hostname is not a valid tag value; declare host=<name> in bench tags")?,
            );
        }

        let mut resources = BTreeMap::new();
        // Which USB device each serial resource resolved to. The coordinator
        // groups resources onto USB/IP channels and cannot see this, so it
        // assumes one device per serial resource; two that share one would get
        // two channels for a single import and the second would never pair.
        let mut serial_devices: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw) in &self.resources {
            let resource = match raw.kind.as_str() {
                "serial" => {
                    let declared =
                        raw.by_path
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

                    // The USB device above the tty is what hiding binds, and
                    // hiding is the only thing keeping this board off this
                    // machine. A walk that fails is therefore a resource that
                    // would register as healthy and then sit there with a live
                    // tty on a bench that has just logged itself hidden — so it
                    // is fatal, not a resource with "no busid to check".
                    let dir =
                        benchd_core::sysfs::usb_device_of_tty(&resolved).with_context(|| {
                            format!(
                                "resource {name:?}: {declared} is {}, which no USB device owns, \
                                 so it cannot be hidden",
                                resolved.display()
                            )
                        })?;
                    // A device forwarded back to the machine it lives on
                    // reproduces the by-id name of the board it came from, so a
                    // by-id resource can resolve to this bench's own imported
                    // copy while a lease is being torn down. Registering that
                    // would hide a phantom and leave the real board on its
                    // driver, visible to everything on this machine.
                    if benchd_core::sysfs::is_forwarded(&dir) {
                        anyhow::bail!(
                            "resource {name:?}: {declared} currently resolves to a device \
                             forwarded in over USB/IP, not to hardware on this machine. \
                             This is normally a lease on this bench being torn down and \
                             clears on its own; declaring the resource by_path avoids it \
                             entirely."
                        );
                    }
                    if let Some(busid) = dir.file_name().and_then(|s| s.to_str()) {
                        if let Some(other) = serial_devices.insert(busid.to_string(), name.clone())
                        {
                            anyhow::bail!(
                                "resources {other:?} and {name:?} are both serial ports on \
                                 USB device {busid}, which is not supported yet: a device is \
                                 forwarded once, and the coordinator would allocate a \
                                 separate channel to each"
                            );
                        }
                    }

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
                    // Recorded now, while the tty still exists. After the
                    // device is hidden there is nothing left to ask.
                    let interface = benchd_core::sysfs::usb_interface_of_tty(&resolved);
                    tracing::info!(
                        %name, path = %resolved.display(),
                        serial = observed.as_deref().unwrap_or("unknown"),
                        interface,
                        "resource present"
                    );
                    Resource::Serial {
                        path,
                        serial: observed,
                        interface,
                    }
                }
                kind @ ("block" | "scsi") => {
                    let busid = raw
                        .busid
                        .as_ref()
                        .with_context(|| format!("resource {name:?} needs busid"))?;
                    // A busid is written by hand, so a typo is likely and would
                    // otherwise surface as a bench that claims fine and then
                    // fails to materialise.
                    let dir = std::path::PathBuf::from("/sys/bus/usb/devices").join(busid);
                    if !dir.exists() {
                        anyhow::bail!(
                            "resource {name:?}: no USB device {busid} on this machine \
                             (unplugged, or a stale busid?)"
                        );
                    }
                    if benchd_core::sysfs::is_forwarded(&std::fs::canonicalize(&dir).unwrap_or(dir))
                    {
                        anyhow::bail!(
                            "resource {name:?}: {busid} is a device forwarded in over USB/IP, \
                             not hardware on this machine"
                        );
                    }
                    tracing::info!(%name, %busid, kind, "resource present");
                    Resource::Usb {
                        busid: busid.clone(),
                        node: if kind == "block" {
                            UsbNode::Block
                        } else {
                            UsbNode::Scsi
                        },
                    }
                }
                "usb" => anyhow::bail!(
                    "resource {name:?}: kind 'usb' no longer says enough — use 'block' for \
                     the storage node or 'scsi' for the control node"
                ),
                other => anyhow::bail!("resource {name:?} has unknown kind {other:?}"),
            };
            resources.insert(name.clone(), resource);
        }

        if self.docs.len() > benchd_core::model::MAX_BENCH_DOCS {
            anyhow::bail!(
                "docs are {} bytes, over the {} byte limit",
                self.docs.len(),
                benchd_core::model::MAX_BENCH_DOCS
            );
        }

        Ok(BenchSpec {
            id: self.id.clone(),
            description: self.description.clone(),
            docs: self.docs.clone(),
            tags,
            resources,
        })
    }
}

pub async fn run(args: HostArgs) -> Result<()> {
    // Releasing comes before anything that can fail, because it runs precisely
    // when things have already gone wrong: after a `SIGKILL`, or as the last
    // step of decommissioning a bench whose config has just been deleted. It
    // needs the bench id and nothing else, so reading and validating a config
    // first only created ways for the recovery path to exit without recovering
    // anything — and boards left stub-bound by a failed release have no tty and
    // no record anybody will read again.
    if args.release_all {
        let bench = release_target(&args)?;
        hide::release_recorded(&hide::state_path(Path::new(&args.state_dir), &bench)).await;
        return Ok(());
    }

    let path = args
        .config
        .as_deref()
        .context("--config is required unless --release-all is given")?;
    let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    let config: BenchConfig =
        toml::from_str(&text).with_context(|| format!("failed to parse {path}"))?;
    check_bench_id(&config.id)?;
    let state = hide::state_path(Path::new(&args.state_dir), &config.id);

    // Release first, always. Anything still hidden from a previous run has to go
    // back to its driver before this one can resolve the bench through it.
    hide::release_recorded(&state).await;

    // A second, best-effort sweep for devices left stubbed by a version that
    // predates the record above. It can only find benches declared `by_id`,
    // because it matches on the USB serial that such a name embeds; `by_path`
    // benches are covered by the record instead.
    {
        let probe = BenchSpec {
            id: config.id.clone(),
            description: String::new(),
            docs: String::new(),
            tags: Vec::new(),
            resources: config
                .resources
                .iter()
                .filter_map(|(name, r)| {
                    r.by_id.as_ref().map(|p| {
                        (
                            name.clone(),
                            Resource::Serial {
                                path: std::path::PathBuf::from(p),
                                serial: None,
                                interface: None,
                            },
                        )
                    })
                })
                .collect(),
        };
        crate::export::recover_orphans(&probe).await;
    }

    // Held outside the work so that shutdown can give the devices back whether
    // we get there by a signal or by falling out of the loop.
    let hidden: Arc<Mutex<Option<Hidden>>> = Arc::new(Mutex::new(None));

    let result = tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
        result = serve_bench(&args, &config, &state, Arc::clone(&hidden)) => result,
    };

    if let Some(hidden) = hidden.lock().await.take() {
        hidden.release().await;
    }
    result
}

/// The bench id becomes a filename under the state directory.
fn check_bench_id(id: &str) -> Result<()> {
    if !benchd_core::model::valid_component(id) {
        anyhow::bail!(
            "bench id {id:?} is not usable as a plain name (letters, digits, dash, \
             underscore, dot)"
        );
    }
    Ok(())
}

/// Which bench `--release-all` is to give devices back to.
///
/// `--bench` is what the unit file should pass, because `%i` is right even when
/// the config it names has been deleted or was never parseable. The config is a
/// fallback for an older unit file, and its own failures are not fatal here:
/// having no config is the case this path exists to survive.
fn release_target(args: &HostArgs) -> Result<String> {
    let from_config = || {
        let text = std::fs::read_to_string(args.config.as_ref()?).ok()?;
        Some(toml::from_str::<BenchConfig>(&text).ok()?.id)
    };
    let Some(bench) = args.bench.clone().or_else(from_config) else {
        anyhow::bail!(
            "--release-all needs to know which bench to release: pass --bench <id> \
             (the systemd instance name), since no bench id could be read from a config"
        );
    };
    check_bench_id(&bench)?;
    Ok(bench)
}

/// SIGTERM or SIGINT.
///
/// Hidden devices are kernel state that outlives this process, so exiting
/// without giving them back leaves a bench with no tty and nothing running that
/// remembers why — the same class of problem as an export that outlives its
/// host (D6). systemd sends SIGTERM, so this is the ordinary shutdown path
/// rather than an edge case.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(err) => {
                tracing::warn!(?err, "cannot listen for SIGTERM; relying on ExecStopPost");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = terminate => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Resolve the bench, hide it, and serve until the hardware goes away.
///
/// Two nested loops, and the difference between them matters: losing the
/// coordinator must **not** unhide the bench, because no lease can exist while
/// it is gone and briefly exposing every board to the machine would undo the
/// point of hiding. Only the hardware itself disappearing sends us back out to
/// resolve again.
async fn serve_bench(
    args: &HostArgs,
    config: &BenchConfig,
    state: &Path,
    hidden: Arc<Mutex<Option<Hidden>>>,
) -> Result<()> {
    loop {
        // Resolved while the ttys still exist, which is the only window there
        // is: hiding removes them, and the busids cannot be recovered from a
        // device that has no tty to walk up from.
        let (spec, busids) = wait_for_hardware(config, args.device_poll_seconds).await;
        tracing::info!(bench = %spec.id, resources = spec.resources.len(), "bench");

        match Hidden::hide(state.to_path_buf(), &busids).await {
            Ok(new) => *hidden.lock().await = Some(new),
            Err(detail) => {
                // Not fatal: a board being re-enumerated is a normal transient,
                // and refusing to start would need a human to come back later.
                tracing::error!(%detail, "could not hide this bench; retrying");
                tokio::time::sleep(Duration::from_secs(args.device_poll_seconds.max(1))).await;
                continue;
            }
        }

        // Nothing this process exported can outlive it: the kernel holds its own
        // reference to a handed-over socket, so teardown is mandatory rather
        // than optional. A socket can also arrive from a previous incarnation,
        // on a binding `hide` adopted instead of making, so clear that before we
        // accept work (D6).
        let mut exports = Exports::new(spec.clone(), args.coordinator.clone(), busids.clone());
        exports.clear_stale().await;

        let ended = loop {
            match session(args, &spec, &busids, &mut exports).await {
                Ok(end @ (SessionEnd::HardwareGone | SessionEnd::Refused)) => break end,
                Ok(SessionEnd::Disconnected) => {
                    tracing::warn!("coordinator closed the connection")
                }
                Err(err) => tracing::warn!(?err, "connection failed"),
            }
            // The coordinator holds all lease state, so anything we were
            // exporting is void the moment we lose it (D6). The bench stays
            // hidden throughout: no lease can exist while the coordinator is
            // gone, and briefly exposing every board would undo the point.
            exports.release_all().await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            tracing::info!("reconnecting");
        };

        exports.release_all().await;
        // Unhidden on both paths out. If the board is gone its `match_busid`
        // entry is not, and that entry would make the stub claim the device the
        // instant it was plugged back in, leaving it with no tty and nothing
        // able to resolve it. If the bench was refused, holding its hardware
        // helps nobody and conceals the cause.
        if let Some(hidden) = hidden.lock().await.take() {
            hidden.release().await;
        }
        match ended {
            SessionEnd::Refused => {
                tracing::error!(
                    "this bench is registered nowhere and its devices have been given \
                     back; fix its config or the coordinator's vocabulary"
                );
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            _ => tracing::warn!("waiting for this bench's hardware to come back"),
        }
    }
}

/// Block until every resource this bench declares is actually present, and
/// resolve the busid of each.
///
/// The alternative — exit and let systemd restart — turns an unplugged board
/// into a restart loop that never ends, and `Restart=always` means it never
/// gives up. Waiting here means a board that is unplugged and plugged back in
/// recovers on its own, and the bench simply is not offered in between.
///
/// The busids are resolved here, with the spec, and a resource that has none is
/// as fatal as a resource that is absent: hiding hides busids, so a bench that
/// registered with one missing would report itself hidden while leaving that
/// board on its driver with a live tty.
async fn wait_for_hardware(
    config: &BenchConfig,
    poll_seconds: u64,
) -> (BenchSpec, BTreeMap<String, String>) {
    let mut complained = false;
    loop {
        match config
            .to_spec()
            .and_then(|spec| Ok((crate::export::busids_for(&spec)?, spec)))
        {
            Ok((busids, spec)) => {
                if complained {
                    tracing::info!(bench = %config.id, "hardware is back");
                }
                return (spec, busids);
            }
            Err(err) => {
                if !complained {
                    // `{:#}` rather than `{}`: the outer message names the
                    // resource and the cause says what is wrong with it, and
                    // without both this is "waiting" with no clue what for.
                    tracing::warn!(
                        bench = %config.id, err = format!("{err:#}"),
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
/// unplugged board left the bench registered and matchable forever, because the
/// host kept heartbeating and, on a same-machine export, never touched the
/// device at all. Every claim then picked the dead bench and failed on the
/// client, and the capability was unusable until someone restarted the host by
/// hand — even with another matching bench free.
///
/// It is also the only thing that unhides a bench. A `match_busid` entry
/// outlives the device it names, so a board that is unplugged while hidden
/// would be claimed by the stub the instant it came back, arriving with no tty
/// and nothing able to resolve it.
///
/// Watches the USB devices, not the ttys. A hidden device is bound to the usbip
/// stub, which detaches it from `cdc_acm` and makes the tty — and the by-path
/// symlink pointing at it — disappear. Polling the tty therefore reported
/// "device lost" a few seconds into every relayed lease and tore it down. The
/// USB device node stays put whichever driver holds it, and vanishes only when
/// the board actually does.
async fn watch_hardware(busids: BTreeMap<String, String>, poll_seconds: u64) -> (String, String) {
    loop {
        tokio::time::sleep(Duration::from_secs(poll_seconds.max(1))).await;
        for (name, busid) in &busids {
            if tokio::fs::metadata(format!("/sys/bus/usb/devices/{busid}"))
                .await
                .is_err()
            {
                return (
                    name.clone(),
                    "the USB device is no longer attached".to_string(),
                );
            }
        }
    }
}

/// Why a coordinator session ended, which decides whether the bench stays
/// hidden.
enum SessionEnd {
    /// The connection went away. Reconnect; the hardware is still ours.
    Disconnected,
    /// The board itself is no longer attached. Give up what we hid and start
    /// over from resolution.
    HardwareGone,
    /// The coordinator refused this bench. Nobody can lease it, so keeping its
    /// devices hidden serves no one and hides the cause as well as the boards.
    Refused,
}

/// One connection to the coordinator, from `Register` until it ends.
async fn session(
    args: &HostArgs,
    spec: &BenchSpec,
    busids: &BTreeMap<String, String>,
    exports: &mut Exports,
) -> Result<SessionEnd> {
    let mut socket = tokio::net::TcpStream::connect(&args.coordinator)
        .await
        .with_context(|| format!("failed to dial {}", args.coordinator))?;
    socket.set_nodelay(true).ok();
    if let Err(error) = benchd_core::protocol::connect(&mut socket).await {
        if error.kind() == std::io::ErrorKind::InvalidData {
            tracing::error!(%error, "coordinator protocol is incompatible");
            return Ok(SessionEnd::Refused);
        }
        return Err(error.into());
    }
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());

    sink.send(serde_json::to_string(&HostMsg::Register {
        bench: spec.clone(),
    })?)
    .await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let heartbeat = {
        let tx = tx.clone();
        let period = Duration::from_secs(args.heartbeat_seconds.max(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Ok(line) = serde_json::to_string(&HostMsg::Heartbeat) else {
                    break;
                };
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    };

    let mut watcher = Box::pin(watch_hardware(busids.clone(), args.device_poll_seconds));

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
                break Ok(SessionEnd::HardwareGone);
            }
            outbound = rx.recv() => {
                let Some(line) = outbound else { break Ok(SessionEnd::Disconnected) };
                sink.send(line).await?;
            }
            inbound = lines.next() => {
                let Some(line) = inbound else { break Ok(SessionEnd::Disconnected) };
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
                        // Retrying will not help until the config changes, and
                        // a bench that matches nothing is worse than no bench
                        // at all. The caller unhides on the way out: a bench
                        // nobody can lease must not also be a bench nobody can
                        // use by hand, or a typo'd tag silently removes the
                        // hardware from the machine.
                        tracing::error!(%reason, "registration refused");
                        break Ok(SessionEnd::Refused);
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

#[cfg(test)]
mod host_tag_tests {
    use super::*;

    #[test]
    fn registration_defaults_to_the_machine_hostname() {
        let config: BenchConfig = toml::from_str("id = 'test'\ntags = []\n[resources]\n").unwrap();
        let spec = config.to_spec().unwrap();
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert!(spec.tags.contains(&benchd_core::tags::Tag::new(
            "host",
            hostname.trim().to_ascii_lowercase()
        )));
    }

    #[test]
    fn an_explicit_host_tag_overrides_the_hostname() {
        let config: BenchConfig =
            toml::from_str("id = 'test'\ntags = ['host=hardware-room']\n[resources]\n").unwrap();
        let spec = config.to_spec().unwrap();
        let hosts: Vec<_> = spec.tags.iter().filter(|t| t.key == "host").collect();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].value, "hardware-room");
    }
}
