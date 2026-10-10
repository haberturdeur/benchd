#!/usr/bin/env bash
# Build benchd for another architecture, without a cross-compiler installed.
#
# deploy.sh builds and installs on the machine it runs on. That covers a bench
# machine that can compile, and not a Raspberry Pi with no Rust toolchain -- so
# a lab's aarch64 hosts are built here and copied there. `cargo build --target`
# alone does not do it: the compile succeeds and the *link* fails, because the
# host's `cc` drives the host's `ld`, which cannot emit aarch64 and says so in
# terms that point at the wrong thing:
#
#   /usr/bin/ld: crt1.o: Relocations in generic ELF (EM: 183)
#   /usr/bin/ld: crt1.o: error adding symbols: file in wrong format
#
# The usual answer is to install a cross-gcc (aarch64-linux-gnu-gcc) and name it
# in .cargo/config.toml. This does not, because the toolchain already ships a
# linker that can do it -- rust-lld -- and requiring a distro package would mean
# one more thing to get right on whichever machine is at hand in an emergency.
# rust-lld is not on PATH and lives at a toolchain-version-dependent path, so it
# is resolved here rather than written down anywhere it could go stale.
#
# `-C link-self-contained=+linker`, which would make this a two-line
# .cargo/config.toml and no script at all, is still unstable on 1.88 and needs
# -Z unstable-options. Revisit when it stabilises.
#
# A *-linux-musl target is what makes this enough on its own: the result is
# statically linked, so there is no target libc to supply and no sysroot to
# assemble. Cross-building a glibc target is a different and larger job.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-aarch64-unknown-linux-musl}"

case "$TARGET" in
  *-linux-musl) ;;
  *)
    echo "refusing to build $TARGET: only *-linux-musl targets are self-contained" >&2
    echo "enough to cross-build with nothing installed. A glibc target needs a" >&2
    echo "sysroot for the target's libc, which this script does not assemble." >&2
    exit 1
    ;;
esac

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "the $TARGET standard library is not installed. Add it with:" >&2
  echo "  rustup target add $TARGET" >&2
  exit 1
fi

# The host triple decides which of the toolchain's bin directories holds the
# linker, and it is not always the triple of the machine you think you are on --
# a musl host, or a toolchain installed for a different ABI, both differ.
HOST=$(rustc -vV | sed -n 's/^host: //p')
SYSROOT=$(rustc --print sysroot)
LLD="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
if [ ! -x "$LLD" ]; then
  echo "no rust-lld at $LLD" >&2
  echo "It ships with the toolchain, so a missing one usually means a partial" >&2
  echo "or unusual rustup install. Either repair it, or install a cross-gcc for" >&2
  echo "$TARGET and set CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER to it." >&2
  exit 1
fi

# cargo reads the linker for a target from this, which spares the repo a
# .cargo/config.toml naming an absolute path that is wrong on every other
# machine and silently stale after a toolchain update on this one.
VAR="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER"
export "$VAR=$LLD"

echo "target: $TARGET"
echo "linker: $LLD"
cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/benchd"
[ -x "$BIN" ] || { echo "cargo built no $BIN" >&2; exit 1; }

# Verify rather than trust, for the same reason deploy.sh does: the failure this
# guards against is a binary that builds, copies and installs cleanly and then
# cannot exec on the machine it was built for. `file` is not everywhere, so fall
# back to reading ELF e_machine, and refuse to claim a check that did not run.
ARCH_WANT=${TARGET%%-*}
if command -v file >/dev/null; then
  DESC=$(file -b "$BIN")
  case "$ARCH_WANT:$DESC" in
    aarch64:*ARM\ aarch64*|x86_64:*x86-64*|armv7:*ARM,\ EABI*|riscv64:*RISC-V*) ;;
    *) echo "WRONG ARCHITECTURE: wanted $ARCH_WANT, built: $DESC" >&2; exit 1 ;;
  esac
  echo "built:  $DESC"
elif command -v readelf >/dev/null; then
  echo "built:  $(readelf -h "$BIN" | sed -n 's/^  Machine: *//p')"
else
  echo "WARNING: neither file nor readelf here; the architecture was NOT checked" >&2
fi

echo "path:   $BIN"
echo "sha256: $(sha256sum "$BIN" | cut -d' ' -f1)"
echo
# --version reports protocol and Git/build identity, but the checksum confirms
# the exact artifact (including different builds of a dirty checkout).
cat <<EOF
To install it on the target machine:

  scp $BIN HOST:/tmp/benchd.new
  ssh HOST '
    sudo cp -a /usr/local/bin/benchd /usr/local/bin/benchd.before-\$(date +%F)
    sudo install -m755 /tmp/benchd.new /usr/local/bin/benchd
    sha256sum /usr/local/bin/benchd
    sudo systemctl restart benchd-coordinator
    for u in \$(systemctl list-units "benchd-host@*" --no-legend | awk "{print \\\$1}"); do
      sudo systemctl restart "\$u"
    done'

Compare that sha256sum against the one above before trusting the result, and
restart the coordinator before the hosts that dial it.
EOF
