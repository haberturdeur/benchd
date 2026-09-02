# benchd

Lease-based hardware bench broker for AI agents.

Agents claim hardware **by capability, for a bounded time**, and get a **real device
node** — after which `esptool`, `idf.py monitor`, `minicom` and `openocd` just work.
Without a lease the device isn't reachable at all, so "the agent forgot to claim" is
an `ENOENT`, not a silently corrupted test run on someone else's board.

```
claim { dut: [soc=esp32s3, psram=octal] }  ttl=15m  reason="wifi reconnect regression"
  → /run/benchd/agents/agent-3/lz7k2/dut/console   ($LAB_DUT_CONSOLE)
```

## Status

Working end-to-end on real hardware, installed as systemd units, including remote
benches forwarded over USB/IP.

| Component | State |
|---|---|
| Tags, vocabulary, implication closure | done, tested |
| Inventory + TOML config | done, tested |
| Matcher (superset match, best-fit, multi-slot, diagnosis) | done — 21 tests + property test |
| Limits (single global set, no roles) | done, tested |
| Lease lifecycle (claim/renew/release/revoke/expire) | done — 15 tests |
| Wire protocol (JSON lines) | done, tested |
| Coordinator daemon | done |
| Host daemon (one per bench) | done |
| Client daemon + bind-mount materialiser | done |
| MCP shim (5 tools) | done |
| Skill | done |
| USB/IP remote benches | done — verified across a real network between two machines |
| Multi-board benches | done, tested |
| Coordinator (leases, reaper) | specified |
| Wire messages (serde enums, JSON lines) | specified |

**Read [`docs/design.md`](docs/design.md) first.** It is authoritative: when the code
and the design doc disagree, the doc wins. It records 19 numbered decisions with the
alternatives that were rejected and why.

## Design in one paragraph

Three components. A **host** owns the hardware of exactly one **bench** (a fixed
physical grouping — possibly several boards on one carrier — always claimed together).
The **coordinator** is the single authority for inventory, matching, limits and lease
state, and the only component that listens. The **client** is a privileged daemon on
each agent machine that materialises device nodes into agent sandboxes, fronted by a
thin per-agent MCP shim. Benches carry `key=value` capability tags from a closed
vocabulary, expanded through an implication graph (`soc=esp32s3` implies
`family=esp32`, `jtag=builtin`, …). A **claim** names one or more **slots**, satisfied
atomically or not at all, possibly from benches on different hosts; among adequate
benches the matcher picks the *least capable* one, scored by the scarcity it would
waste. A granted claim is a **lease** with a mandatory explicit TTL, renewable only by
explicit call, released the moment its session dies. Executors fence on a per-bench
epoch, so a stale instruction can never hand out live hardware.

Everything talks newline-delimited JSON over plain TCP, with no schema language and no
cryptography: benchd assumes a trusted LAN (see §9 of the design doc), and the trust
boundary belongs to the network, not the application.

## Layout

```
docs/design.md                    the design, 19 decisions, and open questions
dist/install.sh                   build, install binaries and systemd units
skill/benchd/SKILL.md             the agent-facing skill
crates/benchd-core/               pure: tags, model, matcher, limits, lease, wire
  tests/matcher.rs                behavioural spec + property test vs a brute-force oracle
  tests/lease.rs                  lease lifecycle spec
  tests/failure_paths.rs          what happens when things go wrong
crates/benchd-coordinator/        the only listener; matching, limits, lease state
crates/benchd-host/               one process per bench; owns the hardware
crates/benchd-client/             privileged: materialises device nodes
crates/benchd-mcp/                per-agent stdio shim, five tools
examples/coordinator.toml         limits + the central vocabulary
examples/bench-*.toml             one file per bench, lives with the hardware
```

`benchd-core` is pure: no I/O, no clock, no sockets. Time is a parameter and decisions
come out as `Effect`s, so the whole lifecycle is testable without daemons or hardware.

## Install

```sh
dist/install.sh          # first time: binaries, config, systemd units
dist/deploy.sh           # thereafter: rebuild, install, verify checksums
sudo systemctl enable --now benchd-coordinator benchd-clientd
sudo systemctl enable --now benchd-host@esp32s3-a      # one per bench
```

Then point an agent at it:

```json
{"mcpServers": {"benchd": {"command": "/usr/local/bin/benchd-mcp",
                           "env": {"BENCHD_IDENTITY": "agent-3"}}}}
```

## Build

```sh
cargo test           # 72 tests, incl. a property test over random inventories
cargo clippy --all-targets
```

Requires Rust 1.88+ (MSRV is pinned by `rmcp`, used once the MCP server lands).

## Reading order

`tests/matcher.rs` is written to be read as the specification — test names and
assertions state what the matcher promises. Start there, then `src/matcher.rs` for
the only module with real algorithmic content.
