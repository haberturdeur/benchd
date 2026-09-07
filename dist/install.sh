#!/usr/bin/env bash
# Install benchd on this machine. Idempotent.
set -euo pipefail
cd "$(dirname "$0")/.."

# Build unless a release binary is already present (so this works under sudo,
# where cargo may not be on PATH).
[ -x target/release/benchd ] || cargo build --release
sudo install -m755 target/release/benchd /usr/local/bin/

sudo mkdir -p /etc/benchd/benches
[ -f /etc/benchd/coordinator.toml ] || sudo install -m644 examples/coordinator.toml /etc/benchd/
for f in examples/bench-*.toml; do
  name=$(basename "$f" .toml); name=${name#bench-}
  [ -f "/etc/benchd/benches/$name.toml" ] || sudo install -m644 "$f" "/etc/benchd/benches/$name.toml"
done

sudo install -m644 dist/systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload

echo
echo "Installed. Now:"
echo "  sudo systemctl enable --now benchd-coordinator benchd-clientd"
for f in /etc/benchd/benches/*.toml; do
  [ -e "$f" ] || continue
  echo "  sudo systemctl enable --now benchd-host@$(basename "$f" .toml)"
done
