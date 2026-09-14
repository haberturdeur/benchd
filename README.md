# benchd

Lease-based hardware bench broker for AI agents.

Agents claim hardware **by capability, for a bounded time**, and get a **real device
node** — after which `esptool`, `idf.py monitor`, `minicom` and `openocd` just work.
Without a lease the device isn't reachable at all, so "the agent forgot to claim" is
an `ENOENT`, not a silently corrupted test run on someone else's board.

```
claim { dut: [soc=esp32s3, psram=octal] }  ttl=15m  reason="wifi reconnect regression"
  → /run/benchd/agent-3/c1-l7/dut/console   ($LAB_DUT_CONSOLE)
```

## Status

Working end-to-end on real hardware, installed as systemd units, including remote
benches forwarded over USB/IP.

| Component | State |
|---|---|
| Tags, vocabulary, implication closure | done, tested |
| Inventory + TOML config | done, tested |
| Matcher (superset match, exact per tag, best-fit, multi-slot, diagnosis) | done — 42 tests incl. a property test |
| Limits (single global set, no roles) | done, tested |
| Lease lifecycle (claim/renew/release/revoke/expire) | done — 15 tests |
| Wire protocol (JSON lines) | done, tested |
| Coordinator daemon | done — 19 tests against a live daemon, hostile input included |
| Host daemon (one per bench) | done |
| Client daemon + device-node materialiser | done |
| MCP shim (5 tools) | done |
| `benchd lease`, the hand-held CLI | done, tested |
| One binary, a subcommand per component | done |
| Skill | done |
| USB/IP remote benches | done — verified across a real network between two machines |
| Multi-board benches | done, tested |
| SSH-tunnelled transport | done — verified between two machines: a board on a second host leased over the forward, control and USB/IP both, with the coordinator bound to loopback and `permitopen` plus the forced command enforced |
| Flashing a remote board | done — unmodified `esptool` writes 256 KB of incompressible data through the forward and reads it back byte-identical, over a board's own USB and over an FT2232H bridge alike |

**Read [`docs/design.md`](docs/design.md) first.** It records 26 numbered decisions with
the alternatives that were rejected and why, and five rounds of adversarial review with
what each one found. It is where an argument is settled — but it is not automatically
right: the most recent review was run *without* it, precisely so the reviewers could
question the decisions rather than check the code against them, and two decisions changed
as a result. When the code and the doc disagree, find out which one is wrong.

## Design in one paragraph

Three components. A **host** owns the hardware of exactly one **bench** (a fixed
physical grouping — possibly several boards on one carrier — always claimed together).
The **coordinator** is the single authority for inventory, matching, limits and lease
state, and the only component that listens. The **client** is a privileged daemon on
each agent machine that materialises a private device node per lease, fronted by a
thin per-agent MCP shim. Benches carry `key=value` capability tags from a closed
vocabulary. What a board *is* expands through an implication graph (`soc=esp32s3`
implies `family=esp32`, `arch=xtensa`, …); how it is *wired* (`console=`, `jtag=`) is
declared per bench, because no chip identity can know which socket the cable is in.
Values may name a specific part without the vocabulary enumerating parts:
`peripheral=accel[mpu6050]` matches a request for either the category or the exact chip.
A **claim** names one or more **slots**, satisfied atomically or not at all, possibly
from benches on different hosts. A bench qualifies when its tags are a superset of what
the slot asks for, each tag compared exactly — nothing knows that `8mb` is more than
`4mb`. Among the benches that qualify the matcher picks the *least capable*, priced by
the capability it would waste and counted over benches that are actually free, since the
scarcity of a board nobody can have is not scarcity. A granted claim is a **lease** with
a mandatory explicit TTL, renewable only by explicit call, released the moment its
session dies. Executors fence on a per-bench epoch, so a stale instruction can never hand
out live hardware.

A client may hold links to **several coordinators at once** — the usual arrangement is a
shared lab server plus one bound to `127.0.0.1` owning the boards on your own desk, which
keeps them private without needing accounts or ACLs, and means a lab outage cannot take
your local hardware with it. Agents are not told: tag counts are summed, statuses merged,
and a claim goes to the first coordinator that can satisfy it.

Everything talks newline-delimited JSON over plain TCP, with no schema language and no
cryptography of its own. The coordinator binds `127.0.0.1` and nothing else; machines
elsewhere reach it through an SSH forward, which is encrypted and mutually authenticated
without benchd knowing it is there. The trust boundary therefore belongs to the
transport rather than to the application, and it is drawn at a whole machine: anything
that can open the tunnel can drive the lab. See §9 of the design doc, which is equally
explicit about what that does *not* cover.

## Layout

```
docs/design.md                    the design, 26 decisions, and open questions
dist/install.sh                   build, install the binary and systemd units
dist/cross-build.sh               build for a host that has no toolchain of its own
skill/benchd/SKILL.md             the agent-facing skill
crates/benchd/                    the one binary: a subcommand per component
crates/benchd-core/               pure: tags, model, matcher, limits, lease, wire
  tests/matcher.rs                behavioural spec + property test vs a brute-force oracle
  tests/lease.rs                  lease lifecycle spec
  tests/failure_paths.rs          what happens when things go wrong
  tests/privileged_input.rs       what the privileged daemons must refuse
crates/benchd/tests/daemons.rs    real daemons, real sockets, hostile messages
crates/benchd-coordinator/        the only listener; matching, limits, lease state
  src/operator.rs                 benches / leases / release
crates/benchd-host/               one process per bench; owns the hardware
crates/benchd-client/             privileged: materialises device nodes
  src/materialize.rs              mknod, ownership, and putting it all back
  src/lease.rs                    hold a bench by hand (unprivileged)
crates/benchd-mcp/                per-agent stdio shim, five tools
examples/coordinator.toml         limits + the central vocabulary
examples/bench-*.toml             one file per bench, lives with the hardware
examples/tunnel.conf              one per coordinator reached from this machine
```

Each component is a library crate; `crates/benchd` is a dispatcher over them and the
only thing that ships. Six binaries meant six versions to keep in step across a lab's
worth of machines, and that is what went wrong; one artefact cannot be half-upgraded
(D26).

`benchd-core` is pure: no I/O, no clock, no sockets. Time is a parameter and decisions
come out as `Effect`s, so the whole lifecycle is testable without daemons or hardware.

## Install

```sh
dist/install.sh          # first time: the binary, config, systemd units
dist/deploy.sh           # thereafter: rebuild, install, verify checksums
sudo systemctl enable --now benchd-coordinator benchd-clientd
sudo systemctl enable --now benchd-host@esp32s3-a      # one per bench
```

The unit names are unchanged; each now runs `benchd coordinator`, `benchd client` or
`benchd host`. A machine installs the same binary whichever of them it runs.

To also use a shared lab server, give the client both — in preference order, so your own
boards are tried first:

```sh
benchd client --coordinator local=127.0.0.1:4711 --coordinator lab=lab.example:4711
```

No sandbox is required: a bench's boards are hidden on the host they are plugged into,
so an agent that reaches for `/dev/ttyUSB0` finds nothing there whether or not it is
confined, and a leased device node belongs to the leasing uid alone. A sandbox buys one
further thing — separating agents that *share* a uid on one machine — and
`dist/benchd-sandbox` was a worked bubblewrap example of that.

**That example is currently broken and is not the way to run agents today.** It gives
each agent a minimal `/dev`, and a lease path is now a symlink into `/dev`, so inside it
every lease dangles. Repairing it needs a per-agent `/dev` carrying that agent's own
leased nodes, which is not written yet. The script says so when run.

Then point an agent at it:

```json
{"mcpServers": {"benchd": {"command": "/usr/local/bin/benchd", "args": ["mcp"],
                           "env": {"BENCHD_IDENTITY": "agent-3"}}}}
```

## Holding a bench by hand

Bringing up a board, checking that a bench is wired the way its notes claim, or just
using the hardware yourself. `benchd lease` claims by capability exactly as an agent
does — it cannot name a bench either — and holds the lease for as long as it runs:

```sh
benchd lease sdmux=usb --ttl 30m        # hold it, print the paths, wait for Ctrl-C
benchd lease soc=esp32s3 -- zsh         # a shell with $LAB_DUT_* already set
benchd lease soc=esp32s3 --slot peer:soc=esp32c3
```

Quitting, being killed, or losing the terminal all hand the hardware straight back,
because the socket connection *is* the session. The TTL is the backstop for when even
that fails.

## Build

```sh
cargo test           # 160 tests, incl. a property test over random inventories
cargo clippy --all-targets --all-features -- -D warnings

# 19 more that spawn real daemons and speak the wire protocol at them,
# ignored by default so a plain `cargo test` stays hermetic and fast
cargo test -p benchd --test daemons -- --ignored --test-threads=1
```

Those live in `crates/benchd` rather than beside the code they exercise, because cargo
only guarantees a freshly built binary to tests in the crate that declares it. Anywhere
else they quietly test whatever was last left in `target/`.

Requires Rust 1.88+ (MSRV is pinned by `rmcp`, which the MCP shim uses).

### For a machine that cannot compile

A bench host is often a Raspberry Pi with no Rust toolchain, so its binary is built
elsewhere and copied over. `cargo build --target` is not enough on its own: the compile
succeeds and the *link* fails, because the host's `cc` drives a linker that cannot emit
the target's architecture.

```sh
dist/cross-build.sh                          # defaults to aarch64-unknown-linux-musl
dist/cross-build.sh x86_64-unknown-linux-musl
```

It uses the `rust-lld` that ships inside the toolchain, so no cross-compiler has to be
installed, and it prints the commands to install the result. A `*-linux-musl` target is
what makes that sufficient — the binary is static, so there is no target libc to supply.

Check the printed `sha256` against the installed file afterwards. Every build reports
version `0.1.0`, so the checksum is the only thing that distinguishes one from another.

## Reading order

`tests/matcher.rs` is written to be read as the specification — test names and
assertions state what the matcher promises. Start there, then `src/matcher.rs` for
the only module with real algorithmic content.
