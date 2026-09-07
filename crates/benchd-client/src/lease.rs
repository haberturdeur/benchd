//! `benchd lease` — hold a bench by hand.
//!
//! The agent surface with a human on the end of it. Everything here is
//! expressible through `benchd mcp`, and deliberately so: this claims by
//! capability like an agent does, cannot name a bench, and gets exactly the
//! five operations agents get (D17). What it adds is a person — someone
//! soldering a wire, bringing up a new board, or checking that a bench is
//! actually wired the way its notes claim.
//!
//! A lease lives as long as this process does. That is the whole design: the
//! socket connection *is* the session, so quitting, being killed, or losing the
//! terminal all give the hardware straight back rather than idling it until a
//! TTL runs out. The TTL is the backstop for the case where even that fails.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use benchd_core::wire::{env_var, ClaimSpec, ClientMsg, RequestId, SessionToken};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

#[derive(Parser)]
#[command(after_help = "\
Examples:
  benchd lease sdmux=usb                       hold an SD-mux bench for 15 minutes
  benchd lease soc=esp32s3 --ttl 1h            ask for longer
  benchd lease soc=esp32s3 -- zsh              a shell with $LAB_DUT_* set
  benchd lease soc=esp32s3 --slot peer:soc=esp32c3
                                               two boards, granted together or not at all

Exit status is 0 when you end the hold yourself, 1 when the lease is lost while
you still wanted it. With a command, it is the command's own status.")]
pub struct LeaseArgs {
    /// Capability tags the bench must have, e.g. `soc=esp32s3 net=wifi`.
    ///
    /// These describe the `dut` slot. Run `benchd benches` to see what exists.
    #[arg(value_name = "TAG")]
    tags: Vec<String>,

    /// Another slot, as `NAME:TAG[,TAG...]` — e.g. `--slot peer:soc=esp32c3`.
    ///
    /// Slots are granted together or not at all, which is what makes two boards
    /// that must talk to each other worth asking for in one go.
    #[arg(long, value_name = "NAME:TAGS")]
    slot: Vec<String>,

    /// How long to hold it: `90s`, `15m`, `1h`.
    ///
    /// Only a backstop — the lease ends when this process does. It matters when
    /// the process dies in a way that takes the socket with it silently.
    #[arg(long, default_value = "15m", value_parser = parse_duration, value_name = "DURATION")]
    ttl: u64,

    /// What you are doing, shown to whoever is waiting for the bench.
    #[arg(long, default_value = "", value_name = "TEXT")]
    reason: String,

    /// The name this hold appears under in `benchd benches`.
    #[arg(long, env = "BENCHD_IDENTITY", value_name = "NAME")]
    name: Option<String>,

    /// The local client daemon's socket.
    #[arg(long, default_value = "/run/benchd/agent.sock", env = "BENCHD_SOCKET")]
    socket: String,

    /// Print the grant as one JSON object instead of prose.
    #[arg(long)]
    json: bool,

    /// Run this with the resource paths in its environment, and release when it
    /// exits. Without one, hold until interrupted.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

// ---------------------------------------------------------------------------
// Argument shapes
// ---------------------------------------------------------------------------

/// The slot every bare tag belongs to. Agents name their slots explicitly; a
/// person asking for one board should not have to.
const DEFAULT_SLOT: &str = "dut";

fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let count: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?} is not a duration; try 90s, 15m, or 1h"))?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        other => return Err(format!("{other:?} is not a unit of time; use s, m, or h")),
    };
    match count.checked_mul(scale) {
        Some(0) => Err("a lease needs a positive duration".into()),
        Some(secs) => Ok(secs),
        None => Err(format!("{text:?} is longer than any lease can be")),
    }
}

/// Turn the bare tags and any `--slot` arguments into a claim's slot map.
fn collect_slots(tags: &[String], extra: &[String]) -> Result<BTreeMap<String, Vec<String>>> {
    let mut slots: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for spec in extra {
        let (name, tags) = spec
            .split_once(':')
            .ok_or_else(|| anyhow!("--slot wants NAME:TAG[,TAG...], not {spec:?}"))?;
        // This becomes a path component under the lease directory, and the
        // daemon that builds that path runs as root. It checks too; so do we.
        if !benchd_core::model::valid_component(name) {
            bail!("{name:?} is not usable as a slot name");
        }
        let tags: Vec<String> = tags
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        if tags.is_empty() {
            bail!("slot {name:?} asks for nothing; give it at least one tag");
        }
        if slots.insert(name.to_string(), tags).is_some() {
            bail!("slot {name:?} was given twice");
        }
    }

    if !tags.is_empty() && slots.insert(DEFAULT_SLOT.into(), tags.to_vec()).is_some() {
        bail!(
            "the bare tags and --slot {DEFAULT_SLOT}: both describe the {DEFAULT_SLOT} slot; \
             use one or the other"
        );
    }

    if slots.is_empty() {
        bail!("nothing was asked for: name at least one tag, e.g. soc=esp32s3");
    }
    Ok(slots)
}

/// A rounded, readable duration. Lease times are minutes, so seconds only
/// matter when there are few of them left.
fn human(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The conversation
// ---------------------------------------------------------------------------

/// The daemon's replies, as much of them as this program needs.
#[derive(Debug, Deserialize)]
struct Grant {
    lease: u64,
    /// slot -> bench id
    slots: BTreeMap<String, String>,
    #[serde(default)]
    docs: BTreeMap<String, String>,
    expires_at: u64,
    /// Set when the granted TTL is shorter than the one asked for.
    #[serde(default)]
    note: Option<String>,
}

/// Where the device nodes ended up: slot -> resource -> path. Arrives after the
/// grant, because a grant is an assignment and the nodes do not exist yet.
#[derive(Debug, Deserialize)]
struct Materialized {
    slots: BTreeMap<String, BTreeMap<String, String>>,
}

/// One connection to the local client daemon.
///
/// A single linear conversation rather than the request router `benchd mcp`
/// needs: MCP tool calls arrive concurrently and have to be correlated, whereas
/// this subcommand says one thing at a time and then waits.
struct Link {
    lines: FramedRead<OwnedReadHalf, LinesCodec>,
    sink: FramedWrite<OwnedWriteHalf, LinesCodec>,
    next: u64,
}

impl Link {
    async fn connect(path: &str) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .with_context(|| {
                format!("could not reach the client daemon at {path} — is `benchd client` running?")
            })?;
        let (read, write) = stream.into_split();
        Ok(Link {
            lines: FramedRead::new(read, LinesCodec::new()),
            sink: FramedWrite::new(write, LinesCodec::new()),
            next: 0,
        })
    }

    async fn send(&mut self, build: impl FnOnce(RequestId) -> ClientMsg) -> Result<RequestId> {
        self.next += 1;
        let request = RequestId(self.next);
        self.sink
            .send(serde_json::to_string(&build(request))?)
            .await?;
        Ok(request)
    }

    async fn recv(&mut self) -> Result<Value> {
        let line = self
            .lines
            .next()
            .await
            .ok_or_else(|| anyhow!("the client daemon closed the connection"))??;
        serde_json::from_str(&line).with_context(|| format!("undecodable reply: {line}"))
    }

    /// Read until the reply to `request` arrives, reporting anything that
    /// happens on the way.
    async fn reply_to(&mut self, request: RequestId) -> Result<Value> {
        loop {
            let value = self.recv().await?;
            if value.get("request").and_then(Value::as_u64) != Some(request.0) {
                announce(&value);
                continue;
            }
            if value.get("msg").and_then(Value::as_str) == Some("error") {
                let detail = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                // Contended and unsatisfiable need different responses from
                // whoever is reading, and only the daemon knows which it is.
                let advice = match value.get("retryable").and_then(Value::as_bool) {
                    Some(true) => {
                        "\nThe hardware exists but is busy; `benchd benches` shows who has it."
                    }
                    _ => {
                        "\nNothing that exists can satisfy this; `benchd benches` shows what does."
                    }
                };
                bail!("{detail}{advice}");
            }
            return Ok(value);
        }
    }
}

/// Unsolicited traffic, printed rather than swallowed: a bench being taken back
/// is the one thing a person holding it needs to know immediately.
fn announce(value: &Value) {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("no reason given")
            .to_string()
    };
    match value.get("msg").and_then(Value::as_str) {
        Some("revoking") => {
            let left = value
                .get("teardown_at")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_sub(now());
            eprintln!(
                "\nthe bench is being taken back in {}: {}\npark the board now",
                human(left),
                text("reason")
            );
        }
        Some("ended") => eprintln!("\nthe lease ended: {}", text("reason")),
        Some("failed") => eprintln!("\nthe lease could not be set up: {}", text("detail")),
        Some("disconnected") => eprintln!("\n{}", text("detail")),
        _ => {}
    }
}

/// True once the lease is gone and the device nodes with it.
fn is_end(value: &Value, lease: u64) -> bool {
    matches!(
        value.get("msg").and_then(Value::as_str),
        Some("ended") | Some("failed")
    ) && value.get("lease").and_then(Value::as_u64) == Some(lease)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// slot -> resource -> path, flattened into the environment agents are told to
/// read. Two identical-looking device nodes are told apart by their names, not
/// by which order they came in.
fn environment(paths: &Materialized) -> BTreeMap<String, String> {
    paths
        .slots
        .iter()
        .flat_map(|(slot, resources)| {
            resources
                .iter()
                .map(move |(name, path)| (env_var(slot, name), path.clone()))
        })
        .collect()
}

fn report(grant: &Grant, paths: &Materialized, held: bool) {
    let left = grant.expires_at.saturating_sub(now());
    println!("lease {} — expires in {}", grant.lease, human(left));
    if let Some(note) = &grant.note {
        println!("note: {note}");
    }

    for (slot, bench) in &grant.slots {
        println!("\n{slot} = {bench}");
        if let Some(resources) = paths.slots.get(slot) {
            for (name, path) in resources {
                println!("  {}={}", env_var(slot, name), path);
            }
        }
    }

    // Whoever wired the bench up wrote these: pinout, jumpers, what is
    // connected to what. There is nowhere else to learn it.
    for (slot, body) in &grant.docs {
        println!("\n--- notes for {slot} ---");
        println!("{}", body.trim_end());
    }

    if held {
        println!("\nholding — Ctrl-C to release");
    }
}

fn report_json(grant: &Grant, paths: &Materialized) {
    let body = serde_json::json!({
        "lease": grant.lease,
        "expires_at": grant.expires_at,
        "benches": grant.slots,
        "paths": paths.slots,
        "env": environment(paths),
        "docs": grant.docs,
    });
    println!("{}", serde_json::to_string(&body).unwrap_or_default());
}

// ---------------------------------------------------------------------------
// Holding
// ---------------------------------------------------------------------------

/// What ended the hold.
enum Ended {
    /// The person asked for it back, or their command finished.
    ByUs(ExitCode),
    /// The lease went away underneath us.
    Lost,
}

/// Wait until the person interrupts us or the lease disappears.
async fn hold(link: &mut Link, lease: u64) -> Ended {
    let (mut interrupt, mut terminate) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(i), Ok(t)) => (i, t),
        _ => return Ended::Lost,
    };
    loop {
        tokio::select! {
            _ = interrupt.recv() => return Ended::ByUs(ExitCode::SUCCESS),
            _ = terminate.recv() => return Ended::ByUs(ExitCode::SUCCESS),
            event = link.recv() => match event {
                Ok(value) => {
                    announce(&value);
                    if is_end(&value, lease) {
                        return Ended::Lost;
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    return Ended::Lost;
                }
            },
        }
    }
}

/// Run a command with the resource paths in its environment, and hold the lease
/// for exactly as long as it runs.
async fn supervise(
    link: &mut Link,
    lease: u64,
    argv: &[String],
    env: &BTreeMap<String, String>,
) -> Ended {
    let mut command = tokio::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    for (key, value) in env {
        command.env(key, value);
    }
    command.env("BENCHD_LEASE", lease.to_string());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("could not run {}: {err}", argv[0]);
            return Ended::ByUs(ExitCode::from(127));
        }
    };

    // Ctrl-C belongs to the command, not to us: it is in the foreground process
    // group too, and a shell's user expects it to interrupt what the shell is
    // running rather than yank the hardware out from under it. Taking the
    // signal and doing nothing with it keeps us alive to release afterwards.
    let mut interrupt = signal(SignalKind::interrupt()).ok();

    loop {
        tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|s| s.code()).unwrap_or(1);
                return Ended::ByUs(ExitCode::from(code.clamp(0, 255) as u8));
            }
            Some(_) = async { match &mut interrupt { Some(s) => s.recv().await, None => None } } => {}
            event = link.recv() => match event {
                Ok(value) => {
                    announce(&value);
                    // Deliberately not killing the command: it may be mid-write
                    // to something that is not the bench, and it will find out
                    // soon enough when the device gives ENOENT.
                    if is_end(&value, lease) {
                        eprintln!("the devices are gone, but the command is still running");
                    }
                }
                Err(err) => eprintln!("{err}"),
            },
        }
    }
}

// ---------------------------------------------------------------------------

pub async fn run(args: LeaseArgs) -> ExitCode {
    match hold_bench(args).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn hold_bench(args: LeaseArgs) -> Result<ExitCode> {
    let slots = collect_slots(&args.tags, &args.slot)?;
    let name = args
        .name
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| format!("hand-{}", std::process::id()));

    let mut link = Link::connect(&args.socket).await?;

    let request = link
        .send(|request| ClientMsg::OpenSession {
            request,
            name: name.clone(),
        })
        .await?;
    link.reply_to(request).await?;

    let request = link
        .send(|request| ClientMsg::Claim {
            request,
            // The daemon substitutes the real token; a process that never
            // handles one cannot present somebody else's.
            session: SessionToken(String::new()),
            claim: ClaimSpec {
                slots,
                ttl: args.ttl,
                reason: args.reason.clone(),
                distinct: true,
            },
        })
        .await?;
    let grant: Grant = serde_json::from_value(link.reply_to(request).await?)
        .context("could not understand the grant")?;

    // A grant is an assignment, not a set of paths: the nodes are imported and
    // mounted afterwards, and only then does the daemon say where they are.
    let paths = loop {
        let value = link.recv().await?;
        if value.get("lease").and_then(Value::as_u64) == Some(grant.lease) {
            match value.get("msg").and_then(Value::as_str) {
                Some("paths") => {
                    break serde_json::from_value::<Materialized>(value)
                        .context("could not understand the device paths")?
                }
                Some("failed") => {
                    let detail = value
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given");
                    bail!(
                        "the bench was granted but could not be set up: {detail}\n\
                         Nothing is held; it is free to try again."
                    );
                }
                _ => {}
            }
        }
        announce(&value);
    };

    let outcome = if args.command.is_empty() {
        if args.json {
            report_json(&grant, &paths);
        } else {
            report(&grant, &paths, true);
        }
        hold(&mut link, grant.lease).await
    } else {
        if args.json {
            report_json(&grant, &paths);
        } else {
            report(&grant, &paths, false);
        }
        supervise(&mut link, grant.lease, &args.command, &environment(&paths)).await
    };

    match outcome {
        Ended::ByUs(code) => {
            // Dropping the connection would release it too, but saying so
            // explicitly means the bench is free before this process exits
            // rather than shortly after.
            let request = link
                .send(|request| ClientMsg::Release {
                    request,
                    session: SessionToken(String::new()),
                    lease: benchd_core::lease::LeaseId(grant.lease),
                })
                .await?;
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(10), link.reply_to(request))
                    .await;
            eprintln!("released");
            Ok(code)
        }
        Ended::Lost => Ok(ExitCode::FAILURE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_written_the_way_people_say_them() {
        assert_eq!(parse_duration("90"), Ok(90));
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("15m"), Ok(900));
        assert_eq!(parse_duration("1h"), Ok(3600));
        assert_eq!(parse_duration(" 2h "), Ok(7200));
    }

    #[test]
    fn a_duration_that_makes_no_sense_says_so() {
        // A zero-length lease is the sort of thing a script computes by
        // accident, and granting it would look like the hardware vanished.
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("soon").is_err());
        // Days look plausible and are not supported; saying which units exist
        // is more use than "invalid value".
        let err = parse_duration("1d").unwrap_err();
        assert!(err.contains("use s, m, or h"), "{err}");
        assert!(parse_duration("99999999999999999999h").is_err());
    }

    #[test]
    fn bare_tags_describe_the_dut() {
        let slots = collect_slots(&["soc=esp32s3".into(), "net=wifi".into()], &[]).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots["dut"], vec!["soc=esp32s3", "net=wifi"]);
    }

    #[test]
    fn extra_slots_are_named_and_may_be_combined_with_bare_tags() {
        let slots = collect_slots(
            &["soc=esp32s3".into()],
            &["peer:soc=esp32c3, net=wifi".into()],
        )
        .unwrap();
        assert_eq!(slots["dut"], vec!["soc=esp32s3"]);
        assert_eq!(slots["peer"], vec!["soc=esp32c3", "net=wifi"]);
    }

    #[test]
    fn a_slot_cannot_be_described_twice() {
        // Both of these mean "the dut", and honouring one silently would drop
        // requirements the caller believes they asked for.
        assert!(collect_slots(&["soc=esp32s3".into()], &["dut:net=wifi".into()]).is_err());
        assert!(collect_slots(&[], &["peer:a=1".into(), "peer:b=2".into()]).is_err());
    }

    #[test]
    fn a_slot_name_cannot_escape_the_lease_directory() {
        // The slot becomes a path component under a root-owned directory.
        for hostile in ["..:a=1", "/etc:a=1", "a/b:a=1", ":a=1"] {
            assert!(collect_slots(&[], &[hostile.into()]).is_err(), "{hostile}");
        }
    }

    #[test]
    fn asking_for_nothing_is_an_error_rather_than_a_wildcard() {
        // Otherwise it would read as "any bench at all", which is exactly the
        // grab-the-first-free-board behaviour claiming by capability replaces.
        assert!(collect_slots(&[], &[]).is_err());
        assert!(collect_slots(&[], &["dut:".into()]).is_err());
    }

    #[test]
    fn the_environment_names_every_resource_by_slot() {
        let paths = Materialized {
            slots: BTreeMap::from([(
                "dut".to_string(),
                BTreeMap::from([
                    (
                        "console".to_string(),
                        "/run/benchd/x/dut/console".to_string(),
                    ),
                    ("sdcard".to_string(), "/run/benchd/x/dut/sdcard".to_string()),
                ]),
            )]),
        };
        let env = environment(&paths);
        assert_eq!(env["LAB_DUT_CONSOLE"], "/run/benchd/x/dut/console");
        assert_eq!(env["LAB_DUT_SDCARD"], "/run/benchd/x/dut/sdcard");
    }

    #[test]
    fn durations_read_back_the_way_they_were_written() {
        assert_eq!(human(45), "45s");
        assert_eq!(human(900), "15m");
        assert_eq!(human(150), "2m 30s");
        assert_eq!(human(3600), "1h");
        assert_eq!(human(3900), "1h 5m");
    }
}
