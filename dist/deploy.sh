#!/usr/bin/env bash
# Build, install, and VERIFY that what is running is what was built.
#
# Four separate debugging dead ends in this project were a stale binary in
# /usr/local/bin: symptoms that looked like flaky code, races, or kernel
# quirks. Never install without checking.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release
BINS="benchd-coordinator benchd-host benchd-clientd benchd-mcp benchd"
sudo install -m755 $(for b in $BINS; do echo -n "target/release/$b "; done) /usr/local/bin/

for b in $BINS; do
  a=$(sha256sum "target/release/$b" | cut -d' ' -f1)
  c=$(sha256sum "/usr/local/bin/$b"  | cut -d' ' -f1)
  [ "$a" = "$c" ] || { echo "MISMATCH: /usr/local/bin/$b is not the binary just built"; exit 1; }
done
# Restart whatever is running, or the verification above is worthless: six
# separate debugging dead ends in this project were a *running process* from an
# older build, twice after this script had already confirmed the files on disk.
UNITS=$(systemctl list-units --plain --no-legend 'benchd-*' 2>/dev/null | awk '{print $1}')
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

echo "deployed and verified: $BINS"
