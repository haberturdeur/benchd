//! Reconcile host units with configured, physically present benches.
//!
//! A running host has deliberately removed its board from `/dev`, so checking
//! config paths directly would call every healthy managed bench absent. The
//! coordinator is the liveness oracle for those: a host registers only after it
//! resolved every resource and hid the devices successfully. Configs not yet
//! represented are started briefly, given time to register, then kept only if
//! they appear in that inventory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use benchd_core::wire::{OperatorMsg, ToOperator, DEFAULT_PORT};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::BenchConfig;

#[derive(Parser)]
pub struct UpdateArgs {
    /// The local coordinator whose registered hosts prove hardware is present.
    #[arg(long, default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"))]
    coordinator: String,

    /// How long newly started hosts get to resolve and register.
    #[arg(long, default_value_t = 6)]
    settle_seconds: u64,
}

#[derive(Debug, PartialEq)]
struct Plan {
    enable: BTreeSet<String>,
    disable: BTreeSet<String>,
}

fn plan(
    configs: &BTreeMap<String, String>,
    registered: &BTreeSet<String>,
    managed: &BTreeSet<String>,
) -> Plan {
    let enable: BTreeSet<String> = configs
        .iter()
        .filter(|(_, id)| registered.contains(*id))
        .map(|(unit, _)| unit.clone())
        .collect();
    let disable = configs
        .keys()
        .filter(|unit| !enable.contains(*unit))
        .chain(managed.iter().filter(|unit| !configs.contains_key(*unit)))
        .cloned()
        .collect();
    Plan { enable, disable }
}

pub async fn run(args: UpdateArgs) -> Result<()> {
    require_root()?;
    let configs = read_configs(Path::new("/etc/benchd/benches"))?;

    // Fail before touching systemd if the authority that decides whether a host
    // successfully came up cannot be inspected.
    inspect(&args.coordinator).await?; // reachability only; inventory is taken after probes settle

    // Already-running hosts keep their leases; starting them again is a no-op
    // at best and a restart at worst. Probe only units that are not up yet.
    let started_by_us: Vec<String> = configs
        .keys()
        .filter(|unit| !is_active(unit))
        .cloned()
        .collect();
    for unit in &started_by_us {
        if let Err(err) = systemctl(&["start", &service(unit)]) {
            for started in &started_by_us {
                let _ = systemctl(&["stop", &service(started)]);
            }
            return Err(err);
        }
    }
    // This is also one full default device-poll interval for already-running
    // hosts. Without it, a board unplugged just before this command could still
    // be registered at the instant we inspect and be incorrectly retained.
    if !configs.is_empty() {
        tokio::time::sleep(Duration::from_secs(args.settle_seconds)).await;
    }

    let state = match inspect(&args.coordinator).await {
        Ok(state) => state,
        Err(err) => {
            // Do not leave probes running forever merely because the
            // coordinator vanished between the two inspections.
            for unit in &started_by_us {
                let _ = systemctl(&["stop", &service(unit)]);
            }
            return Err(err.context("coordinator disappeared while probing benches"));
        }
    };
    let registered: BTreeSet<String> = state.into_iter().map(|bench| bench.id).collect();
    let managed_now = managed_units()?;
    let plan = plan(&configs, &registered, &managed_now);

    for unit in &plan.enable {
        systemctl(&["enable", "--now", &service(unit)])?;
    }
    for unit in &plan.disable {
        systemctl(&["disable", "--now", &service(unit)])?;
        // A missing or invalid config can leave an instance failed even after
        // it is stopped; stale red units make the next real failure invisible.
        let _ = systemctl(&["reset-failed", &service(unit)]);
    }

    for unit in &plan.enable {
        println!("{unit}: enabled (hardware present)");
    }
    for unit in &plan.disable {
        let reason = match configs.get(unit) {
            Some(_) => "hardware not present",
            None => "config removed",
        };
        println!("{unit}: disabled ({reason})");
    }
    if plan.enable.is_empty() && plan.disable.is_empty() {
        println!("no bench units to update");
    }
    Ok(())
}

/// Config filename and `id` must agree. ExecStopPost releases by the instance
/// name even when the config has gone; allowing two names for one bench would
/// make cleanup look in the wrong hidden-device ledger.
fn read_configs(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut configs = BTreeMap::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("could not read bench config directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|part| part.to_str()) != Some("toml") {
            continue;
        }
        let unit = path
            .file_stem()
            .and_then(|part| part.to_str())
            .ok_or_else(|| anyhow!("{} has no usable filename", path.display()))?
            .to_string();
        if !benchd_core::model::valid_component(&unit) {
            bail!(
                "{} is not usable as a systemd instance name",
                path.display()
            );
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let config: BenchConfig =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        if config.id != unit {
            bail!(
                "{} declares id {:?}; the filename and id must agree so cleanup releases \
                 the right hidden devices",
                path.display(),
                config.id
            );
        }
        configs.insert(unit, config.id);
    }
    Ok(configs)
}

/// Everything systemd currently knows about, active or failed. Enabled but
/// never-loaded instances are found separately through the wants directory.
fn managed_units() -> Result<BTreeSet<String>> {
    let mut units = BTreeSet::new();
    let output = Command::new("systemctl")
        .args([
            "list-units",
            "benchd-host@*.service",
            "--all",
            "--plain",
            "--no-legend",
            "--no-pager",
        ])
        .output()
        .context("could not run systemctl list-units")?;
    if !output.status.success() {
        bail!("systemctl list-units failed");
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(unit) = line.split_whitespace().next().and_then(instance_of) {
            units.insert(unit.to_string());
        }
    }

    let wants = Path::new("/etc/systemd/system/multi-user.target.wants");
    if let Ok(entries) = std::fs::read_dir(wants) {
        for entry in entries.flatten() {
            if let Some(unit) = entry
                .file_name()
                .to_str()
                .and_then(instance_of)
                .map(str::to_string)
            {
                units.insert(unit);
            }
        }
    }
    Ok(units)
}

fn instance_of(name: &str) -> Option<&str> {
    let instance = name
        .strip_prefix("benchd-host@")?
        .strip_suffix(".service")?;
    benchd_core::model::valid_component(instance).then_some(instance)
}

fn service(instance: &str) -> String {
    format!("benchd-host@{instance}.service")
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("could not run systemctl {}", args.join(" ")))?;
    if !status.success() {
        bail!("systemctl {} failed with {status}", args.join(" "));
    }
    Ok(())
}

fn is_active(instance: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", &service(instance)])
        .status()
        .is_ok_and(|status| status.success())
}

fn require_root() -> Result<()> {
    let status =
        std::fs::read_to_string("/proc/self/status").context("could not read process uid")?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok());
    if effective != Some(0) {
        bail!("update-benches manages system services; run it with sudo");
    }
    Ok(())
}

async fn inspect(coordinator: &str) -> Result<Vec<benchd_core::wire::BenchView>> {
    let mut socket = tokio::net::TcpStream::connect(coordinator)
        .await
        .with_context(|| format!("failed to reach the coordinator at {coordinator}"))?;
    benchd_core::protocol::connect(&mut socket).await?;
    let (read, write) = socket.into_split();
    let mut lines = FramedRead::new(read, LinesCodec::new());
    let mut sink = FramedWrite::new(write, LinesCodec::new());
    sink.send(serde_json::to_string(&OperatorMsg::Inspect)?)
        .await?;
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next())
        .await
        .map_err(|_| anyhow!("the coordinator did not reply within 10s"))?
        .ok_or_else(|| anyhow!("the coordinator closed the connection"))??;
    match serde_json::from_str::<ToOperator>(&line)? {
        ToOperator::State { benches, .. } => Ok(benches),
        ToOperator::Error { error } => Err(anyhow!(error)),
        other => bail!("unexpected coordinator reply: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{instance_of, plan, read_configs};
    use std::collections::{BTreeMap, BTreeSet};

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn reconciliation_is_the_intersection_of_configs_and_registered_hardware() {
        let configs = BTreeMap::from([
            ("connected".into(), "connected".into()),
            ("unplugged".into(), "unplugged".into()),
        ]);
        let got = plan(
            &configs,
            &set(&["connected"]),
            &set(&["connected", "old-name"]),
        );
        assert_eq!(got.enable, set(&["connected"]));
        assert_eq!(got.disable, set(&["old-name", "unplugged"]));
    }

    #[test]
    fn a_running_hidden_bench_is_kept_because_it_is_registered() {
        let configs = BTreeMap::from([("hidden".into(), "hidden".into())]);
        let got = plan(&configs, &set(&["hidden"]), &set(&["hidden"]));
        assert_eq!(got.enable, set(&["hidden"]));
        assert!(got.disable.is_empty());
    }

    #[test]
    fn only_host_instances_are_parsed() {
        assert_eq!(instance_of("benchd-host@semafor.service"), Some("semafor"));
        assert_eq!(instance_of("benchd-host@.service"), None);
        assert_eq!(instance_of("benchd-clientd.service"), None);
    }

    #[test]
    fn a_config_whose_id_does_not_match_its_filename_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "benchd-update-mismatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("semafor.toml"),
            "id = \"other\"\ntags = []\n[resources]\n",
        )
        .unwrap();
        let err = read_configs(&dir).unwrap_err().to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("filename and id must agree"), "{err}");
    }

    #[test]
    fn a_matching_config_is_accepted() {
        let dir = std::env::temp_dir().join(format!(
            "benchd-update-ok-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("semafor.toml"),
            "id = \"semafor\"\ntags = []\n[resources]\n",
        )
        .unwrap();
        let configs = read_configs(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            configs,
            BTreeMap::from([("semafor".into(), "semafor".into())])
        );
    }
}
