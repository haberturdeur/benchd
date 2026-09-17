#!/usr/bin/env bash
# Build, install, and VERIFY that what is running is what was built.
#
# Four separate debugging dead ends in this project were a stale binary in
# /usr/local/bin: symptoms that looked like flaky code, races, or kernel
# quirks. Never install without checking.
set -euo pipefail
cd "$(dirname "$0")/.."

# The five binaries this one replaced.
SUPERSEDED="benchd-coordinator benchd-host benchd-clientd benchd-mcp benchd-lease"

# Drop-ins are checked before anything is built, installed or deleted.
#
# A drop-in under /etc/systemd/system/benchd-*.service.d/ survives an
# `install -m644` of the unit it extends and still applies afterwards. One that
# overrides ExecStart= therefore still names whatever binary it named when it
# was written -- and if that is one of the five below, the deletion further
# down turns the next start into 203/EXEC. Nothing in the shipped units can
# tell us about it, so look, and stop: a machine running an old binary is more
# use than a machine running none, and the human who has to fix the drop-in is
# the same human either way.
STALE=""
FORCE_RELAY=""
for conf in /etc/systemd/system/benchd-*.service.d/*.conf /etc/systemd/system/benchd-*.service; do
  [ -e "$conf" ] || continue
  # A unit this deploy ships is about to be correct by definition, so its
  # current contents say nothing. What matters is what it does not ship: a
  # drop-in, or a hand-written unit nobody remembers.
  if [ -e "dist/systemd/$(basename "$conf")" ]; then
    continue
  fi
  # Comments do not invoke anything, and a commented-out ExecStart from the
  # migration is exactly the sort of thing that would be in one.
  body=$(grep -vE '^[[:space:]]*[#;]' "$conf" || true)
  for old in $SUPERSEDED; do
    # Bounded on both sides so that `Requires=benchd-coordinator.service` and
    # `benchd-host@.service` -- unit names, not executables -- do not match.
    if grep -qE "(^|[[:space:]]|[=/])$old([[:space:]]|$)" <<<"$body"; then
      STALE="$STALE $conf:$old"
    fi
  done
  # --force-relay was deleted in the merge with no deprecation period, and clap
  # answers an unknown flag with a usage error and exit 2 -- under
  # Restart=always, a restart loop whose journal says nothing about which flag
  # is at fault. Nothing in dist/ passes it; a drop-in or an alias might.
  if grep -q -- '--force-relay' <<<"$body"; then
    FORCE_RELAY="$FORCE_RELAY $conf"
  fi
done
if [ -n "$FORCE_RELAY" ]; then
  echo "WARNING: --force-relay no longer exists and makes the unit exit 2 on every start:"
  for conf in $FORCE_RELAY; do echo "  $conf"; done
  echo "         relaying is decided per lease now; drop the flag."
fi
if [ -n "$STALE" ]; then
  echo "REFUSING to deploy: a drop-in still invokes a binary this deploy removes" >&2
  for entry in $STALE; do echo "  ${entry%:*} names ${entry##*:}" >&2; done
  echo >&2
  echo "Rewrite it to 'benchd <subcommand>' first. An ExecStart= override needs" >&2
  echo "an empty ExecStart= line before the new one, or systemd appends instead" >&2
  echo "of replacing." >&2
  exit 1
fi

cargo build --release

# Ask cargo where it built rather than assuming ./target. CARGO_TARGET_DIR in
# the environment, or build.target-dir in a .cargo/config.toml, sends the
# artefact elsewhere -- and then the install and the checksum below both read a
# *different*, older file and agree with each other about it. That is how this
# script reported "deployed and verified" for a binary the build never touched,
# which is precisely the failure the rest of it exists to catch.
TARGET_DIR=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | grep -o '"target_directory":"[^"]*"' | head -1 | cut -d'"' -f4)
BIN="${TARGET_DIR:-${CARGO_TARGET_DIR:-target}}/release/benchd"
[ -x "$BIN" ] || { echo "cargo built no $BIN" >&2; exit 1; }

sudo install -m755 "$BIN" /usr/local/bin/
sudo install -m755 dist/benchd-sandbox /usr/local/bin/

a=$(sha256sum "$BIN" | cut -d' ' -f1)
c=$(sha256sum /usr/local/bin/benchd  | cut -d' ' -f1)
[ "$a" = "$c" ] || { echo "MISMATCH: /usr/local/bin/benchd is not the binary just built"; exit 1; }

# The five binaries this one replaced, deleted rather than left to rot. They
# still run and still answer, so a unit file or an MCP config that was missed in
# the migration keeps working — against a build from before the merge. That is
# the same stale-binary trap as above, wearing a different name.
for old in $SUPERSEDED; do
  if [ -e "/usr/local/bin/$old" ]; then
    sudo rm -f "/usr/local/bin/$old"
    echo "removed superseded /usr/local/bin/$old"
  fi
done
# Unit files too. They name the binary and its subcommand, so a deploy that
# refreshes only the binary is exactly as broken as one that refreshes only the
# units — which is how the D26 migration would have failed: correct binary in
# place, every unit still invoking a name that no longer exists.
#
# Unconditional, because the deletion above is. Gating this on `systemctl
# list-units` -- which lists *loaded* units -- meant a machine whose units are
# installed but not enabled got the deletion and not the refresh: one being
# staged, or one where a host was disabled to swap a board, left with
# benchd-host@.service still invoking /usr/local/bin/benchd-host and 203/EXEC
# waiting for whoever started it next. Installing a unit file on a machine with
# nothing running is harmless; leaving one behind is not.
#
# Site-local edits do not survive it, and that is a real loss: the shipped
# units hard-code BENCHD_COORDINATOR=127.0.0.1:4711, so any lab with a shared
# coordinator has to change it, and editing the unit is the obvious way. Keep a
# copy and say so. install.sh creates a drop-in for exactly this override,
# which nothing here writes to.
for shipped in dist/systemd/*.service; do
  installed=/etc/systemd/system/$(basename "$shipped")
  [ -f "$installed" ] || continue
  if ! cmp -s "$shipped" "$installed"; then
    backup="$installed.local-$(date +%Y%m%d%H%M%S)"
    sudo cp -a "$installed" "$backup"
    echo "WARNING: $installed had local changes; kept them at $backup"
    echo "         (systemd ignores that name) — move them into"
    echo "         ${installed}.d/10-local.conf, which no deploy overwrites"
  fi
done
sudo install -m644 dist/systemd/*.service /etc/systemd/system/
# The tunnel unit names /etc/benchd/tunnels/%i.conf, and this script installs
# that unit on machines install.sh may never have run on. Created here too, and
# with the mode rather than without it: ssh refuses a private key that other
# users can read, so a directory made by hand at the default 0755 fails at
# connect time on the machine that has just lost its route to the lab.
sudo mkdir -p -m700 /etc/benchd/tunnels
sudo systemctl daemon-reload

# Is this unit running *now*? `systemctl restart` on an inactive unit starts
# it, and a unit can be installed and deliberately not enabled: a host-only
# machine keeps benchd-coordinator.service on disk and never runs it, and the
# old unconditional restart started one listening on 0.0.0.0:4711 with no
# authentication on every deploy. Restart only what was already running.
running() {
  case "$(systemctl is-active "$1" 2>/dev/null)" in
    active|activating|reloading) return 0 ;;
    *) return 1 ;;
  esac
}

# When this unit's current main process started, in monotonic microseconds.
main_start() {
  systemctl show --property=ExecMainStartTimestampMonotonic --value "$1" 2>/dev/null || true
}

# Did this unit actually come back? Not the same question as running(), which
# counts `activating`: a unit in a Restart=always loop is activating most of the
# time and briefly active in between, so is-active alone would call a crash
# loop a success about a third of the time. So also require that the process
# running now is the one our restart started — a unit that has replaced it
# since did so on its own, which is what a crash loop is.
came_back() {
  [ "$(systemctl is-active "$1" 2>/dev/null)" = active ] || return 1
  [ "$(main_start "$1")" = "${STARTED_AT[$1]-}" ]
}

# Restart whatever is running, or the verification above is worthless: six
# separate debugging dead ends in this project were a *running process* from an
# older build, twice after this script had already confirmed the files on disk.
#
# --all, so that a unit which is loaded but stopped is considered and then
# skipped by running() above, rather than never being looked at.
UNITS=$(systemctl list-units --all --plain --no-legend 'benchd-*' 2>/dev/null | awk '{print $1}')
RESTARTED=""
FAILED=""
declare -A STARTED_AT

restart() {
  if sudo systemctl restart "$1"; then
    RESTARTED="$RESTARTED $1"
    STARTED_AT[$1]=$(main_start "$1")
  else
    FAILED="$FAILED $1"
  fi
}

# Coordinator first: hosts and clients reconnect to it.
if running benchd-coordinator.service; then
  restart benchd-coordinator.service
  # Wait for it to be listening rather than merely started. The unit passes
  # --report-address, which is written only once the listener is accepting, and
  # RuntimeDirectory= deletes it on stop -- so the file reappearing is the
  # signal. The fixed `sleep 1` this replaces was a guess, and a host restarted
  # against a coordinator that has not bound yet pays a full 3s reconnect
  # cycle for it.
  for _ in $(seq 1 100); do
    if [ -s /run/benchd-coordinator/address ]; then
      break
    fi
    sleep 0.1
  done
  if [ ! -s /run/benchd-coordinator/address ]; then
    echo "WARNING: benchd-coordinator did not report a bound address within 10s"
  fi
fi

for u in $UNITS; do
  case "$u" in
    benchd-coordinator.service) continue;;
    # A tunnel runs ssh, not benchd, so a new binary is no reason to drop the
    # forward -- and dropping it would cut every host and client on this machine
    # off from the lab in the middle of a deploy, to install code the unit does
    # not contain. It would also make a tunnel that failed for reasons of its own
    # -- a lab server rebooting, a network blip -- get reported below as a failed
    # deploy, which is exactly the wrong place to go looking.
    benchd-tunnel@*.service) continue;;
  esac
  running "$u" || continue
  restart "$u"
done

# A restart job succeeding means the process was forked, nothing more. A unit
# that exits 200ms later -- a subcommand the binary does not have, a config it
# cannot parse -- is back on its RestartSec timer, and every restart here used
# to be `|| true` followed unconditionally by "deployed and verified". This
# script's whole premise is that it checks; a settle window and a second look
# are what make that true of the processes as well as the files.
#
# Longer than RestartSec=3 on purpose, so that a unit which is going to die has
# already done it and been counted by the time we look.
if [ -n "$RESTARTED" ]; then
  echo "restarted:$RESTARTED"
  sleep 4
  for u in $RESTARTED; do
    came_back "$u" || FAILED="$FAILED $u"
  done
fi

if [ -n "$FAILED" ]; then
  echo >&2
  echo "FAILED to come back:$FAILED" >&2
  for u in $FAILED; do
    systemctl --no-pager --lines=15 status "$u" >&2 || true
  done
  exit 1
fi

echo "deployed and verified: benchd"
