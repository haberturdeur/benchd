#!/usr/bin/env bash
# Install benchd on this machine. Idempotent.
set -euo pipefail
cd "$(dirname "$0")/.."

# The subcommands the unit files invoke. Every one of them has to exist in the
# binary this script installs, or the units die on `unrecognized subcommand`.
SUBCOMMANDS="coordinator host client"

# Ask cargo where it builds rather than assuming ./target. CARGO_TARGET_DIR in
# the environment, or build.target-dir in a .cargo/config.toml, sends the
# artefact somewhere else -- and then every check below reads a *different*,
# older file, compares it against itself, and reports a successful install of a
# binary the build never touched. Exactly the stale-artefact failure the rest of
# this script exists to prevent, arriving through the one path it trusted.
TARGET_DIR=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | grep -o '"target_directory":"[^"]*"' | head -1 | cut -d'"' -f4)
BIN="${TARGET_DIR:-${CARGO_TARGET_DIR:-target}}/release/benchd"

# Ask a binary whether it is the merged benchd.
#
# `--help` on a subcommand parses the command line and exits without running
# anything, so this costs nothing and touches no hardware.
has_subcommands() {
  local binary=$1 sub
  for sub in $SUBCOMMANDS; do
    "$binary" "$sub" --help >/dev/null 2>&1 || return 1
  done
}

# Build unless a release binary is already present (so this works under sudo,
# where cargo may not be on PATH) -- but "present" is not "correct". `benchd`
# was a binary name before the merge too: it was the operator CLI, and cargo
# never deletes a binary it has stopped producing. So a release `benchd`
# exists on every machine that ever built the old tree, and a plain -x test on
# it made `git pull && dist/install.sh` skip the build, install a program with
# no coordinator, host or client subcommand, and then write unit files invoking
# all three -- every unit dead on arrival under Restart=always, reading for all
# the world like a broken build rather than a five-month-old artefact.
#
# Asked of the binary rather than compared against a checksum because before
# the build has run there is nothing to compare against, and rather than built
# unconditionally because that would lose the no-cargo-on-PATH case above. What
# actually matters is whether the file can do the job the units ask of it.
if ! has_subcommands "$BIN"; then
  if ! command -v cargo >/dev/null; then
    echo "$BIN is missing or predates the one-binary merge," >&2
    echo "and cargo is not on PATH to rebuild it. Run 'cargo build --release'" >&2
    echo "as the user who owns the build tree, then run this script again." >&2
    exit 1
  fi
  cargo build --release
  has_subcommands "$BIN" || {
    echo "the build produced a $BIN without one of:" >&2
    echo "  $SUBCOMMANDS" >&2
    exit 1
  }
fi

sudo install -m755 "$BIN" /usr/local/bin/

# The check deploy.sh has always had, and this script never did. Four separate
# debugging dead ends in this project were a stale binary in /usr/local/bin,
# and the one above only proves the *source* of the copy was sound.
a=$(sha256sum "$BIN" | cut -d' ' -f1)
c=$(sha256sum /usr/local/bin/benchd  | cut -d' ' -f1)
[ "$a" = "$c" ] || { echo "MISMATCH: /usr/local/bin/benchd is not the binary just built"; exit 1; }

# An earlier benchd elsewhere on PATH shadows the one just installed for
# anybody typing the command by hand. The units name an absolute path and are
# unaffected, which is what makes this worth saying out loud: the daemons would
# be right and the operator's own `benchd leases` wrong.
on_path=$(command -v benchd 2>/dev/null || true)
if [ -n "$on_path" ] && [ "$on_path" != /usr/local/bin/benchd ]; then
  echo "WARNING: $on_path comes before /usr/local/bin/benchd on your PATH"
fi

sudo mkdir -p /etc/benchd/benches
# Private keys live here, so this one is not world-readable like the rest of
# /etc/benchd. ssh refuses to use a key other users can read, so getting this
# wrong fails at connect time rather than quietly widening the boundary -- but
# it fails on the machine that can no longer reach the lab, which is a poor
# place to learn it.
sudo mkdir -p -m700 /etc/benchd/tunnels
[ -f /etc/benchd/coordinator.toml ] || sudo install -m644 examples/coordinator.toml /etc/benchd/
# Not a live config: benchd-tunnel@<name> reads <name>.conf, and no instance
# name produces this one. It is here to be copied and edited.
sudo install -m600 examples/tunnel.conf /etc/benchd/tunnels/tunnel.conf.example
for f in examples/bench-*.toml; do
  name=$(basename "$f" .toml); name=${name#bench-}
  [ -f "/etc/benchd/benches/$name.toml" ] || sudo install -m644 "$f" "/etc/benchd/benches/$name.toml"
done

# This script is idempotent and gets re-run, and the units have to be refreshed
# when it is: they name the binary's subcommands, so a unit older than the
# binary invokes something that no longer exists. An edit made in place does not
# survive that, so keep a copy of it under a name systemd ignores rather than
# discarding a lab's coordinator address without a word.
for shipped in dist/systemd/*.service; do
  installed=/etc/systemd/system/$(basename "$shipped")
  if [ -f "$installed" ] && ! cmp -s "$shipped" "$installed"; then
    backup="$installed.local-$(date +%Y%m%d%H%M%S)"
    sudo cp -a "$installed" "$backup"
    echo "WARNING: $installed had local changes; kept them at $backup"
  fi
done
sudo install -m644 dist/systemd/*.service /etc/systemd/system/

# A drop-in per unit that needs an address, created with the same default the
# unit already has and then never touched again. The shipped units carry that
# default themselves so a single-machine install needs no configuration, but a
# lab with a shared coordinator has to change it -- and the obvious place to
# make that change is the unit, which this script and deploy.sh both overwrite.
# Giving the edit a home that neither of them writes to is the difference
# between a documented override and a host that quietly went back to talking to
# itself.
for unit in benchd-host@.service benchd-clientd.service; do
  conf=/etc/systemd/system/$unit.d/10-coordinator.conf
  if [ -e "$conf" ]; then
    continue
  fi
  sudo mkdir -p "$(dirname "$conf")"
  sudo tee "$conf" >/dev/null <<'EOF'
# Local override. Nothing in dist/ writes to this file again; edit it freely.
#
# A shared lab coordinator is reached through an SSH tunnel, so this address
# stays on loopback: it names the local end of the forward, not the lab. Write
# /etc/benchd/tunnels/lab.conf (copy tunnel.conf.example) and then
#   systemctl enable --now benchd-tunnel@lab
#
# Naming a remote address here directly still works, but only against a
# coordinator deliberately bound off loopback with --listen, and then nothing
# authenticates the link or encrypts what crosses it.
#
# A client that names several coordinators has already replaced ExecStart= with
# literal addresses, and this variable is then never read -- the tunnel address
# belongs in that override instead. Note also that drop-ins apply in lexical
# order and the last ExecStart= wins, so a file named to sort *before* the one
# it means to override is applied and then silently discarded.
[Service]
Environment=BENCHD_COORDINATOR=127.0.0.1:4711
EOF
  echo "created $conf"
done

sudo systemctl daemon-reload

echo
echo "Installed. Now:"
echo "  sudo systemctl enable --now benchd-coordinator benchd-clientd"
for f in /etc/benchd/benches/*.toml; do
  [ -e "$f" ] || continue
  echo "  sudo systemctl enable --now benchd-host@$(basename "$f" .toml)"
done

# Only worth saying on a machine that has actually configured a tunnel: the
# unit names /usr/bin/ssh, and a missing client turns into a unit that fails
# three seconds after every restart with nothing pointing at the cause.
for f in /etc/benchd/tunnels/*.conf; do
  [ -e "$f" ] || continue
  command -v ssh >/dev/null || echo "WARNING: $f exists but ssh is not installed"
  break
done

echo
echo "The coordinator binds 127.0.0.1 only. To reach one on another machine,"
echo "forward to it rather than naming it directly:"
echo "  sudo cp /etc/benchd/tunnels/tunnel.conf.example /etc/benchd/tunnels/lab.conf"
echo "  sudo \$EDITOR /etc/benchd/tunnels/lab.conf        # target, key, known hosts"
echo "  sudo systemctl enable --now benchd-tunnel@lab"
echo
echo "The daemons go on talking to 127.0.0.1:4711, which is the near end of"
echo "that forward. Their addresses stay yours to edit:"
echo "  /etc/systemd/system/benchd-host@.service.d/10-coordinator.conf"
echo "  /etc/systemd/system/benchd-clientd.service.d/10-coordinator.conf"
