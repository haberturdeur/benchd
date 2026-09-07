#!/usr/bin/env bash
# Build, install, and VERIFY that what is running is what was built.
#
# Four separate debugging dead ends in this project were a stale binary in
# /usr/local/bin: symptoms that looked like flaky code, races, or kernel
# quirks. Never install without checking.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
sudo install -m755 target/release/benchd /usr/local/bin/

a=$(sha256sum target/release/benchd | cut -d' ' -f1)
c=$(sha256sum /usr/local/bin/benchd  | cut -d' ' -f1)
[ "$a" = "$c" ] || { echo "MISMATCH: /usr/local/bin/benchd is not the binary just built"; exit 1; }

# The five binaries this one replaced, deleted rather than left to rot. They
# still run and still answer, so a unit file or an MCP config that was missed in
# the migration keeps working — against a build from before the merge. That is
# the same stale-binary trap as above, wearing a different name.
for old in benchd-coordinator benchd-host benchd-clientd benchd-mcp benchd-lease; do
  if [ -e "/usr/local/bin/$old" ]; then
    sudo rm -f "/usr/local/bin/$old"
    echo "removed superseded /usr/local/bin/$old"
  fi
done
# Unit files too. They name the binary and its subcommand, so a deploy that
# refreshes only the binary is exactly as broken as one that refreshes only the
# units — which is how the D26 migration would have failed: correct binary in
# place, every unit still invoking a name that no longer exists.
UNITS=$(systemctl list-units --plain --no-legend 'benchd-*' 2>/dev/null | awk '{print $1}')
if [ -n "$UNITS" ]; then
  sudo install -m644 dist/systemd/*.service /etc/systemd/system/
  sudo systemctl daemon-reload
fi

# Restart whatever is running, or the verification above is worthless: six
# separate debugging dead ends in this project were a *running process* from an
# older build, twice after this script had already confirmed the files on disk.
if [ -n "$UNITS" ]; then
  # Coordinator first: hosts and clients reconnect to it.
  sudo systemctl restart benchd-coordinator 2>/dev/null || true
  sleep 1
  for u in $UNITS; do
    case "$u" in benchd-coordinator.service) continue;; esac
    sudo systemctl restart "$u" 2>/dev/null || true
  done
  echo "restarted: $(echo $UNITS | tr '\n' ' ')"
fi

echo "deployed and verified: benchd"
