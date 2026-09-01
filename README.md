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

Early. The matcher is implemented and tested; the daemon is not built yet.

| Component | State |
|---|---|
| Tags, vocabulary, implication closure | done, tested |
| Inventory + TOML config | done, tested |
| Matcher (superset match, best-fit, multi-slot, diagnosis) | done — 21 tests + property test |
| Multi-board benches | done, tested |
| Policy engine | specified |
| Coordinator (leases, reaper, reconcile) | specified |
| Host (one per bench) | specified |
| Client (MCP + sandbox materialiser) | specified |
| Wire protocol | undecided |

**Read [`docs/design.md`](docs/design.md) first.** It is authoritative: when the code
and the design doc disagree, the doc wins. It records 20 numbered decisions with the
alternatives that were rejected and why.

## Design in one paragraph

Three components. A **host** owns the hardware of exactly one **bench** (a fixed
physical grouping — possibly several boards on one carrier — that is always claimed
together). The **coordinator** is the single authority: inventory, matching, policy,
lease state, reaper. The **client** is a privileged daemon on each agent machine that
serves MCP and materialises device nodes into agent sandboxes. Benches carry
`key=value` capability tags from a closed vocabulary, expanded through an implication
graph (`soc=esp32s3` implies `family=esp32`, `jtag=builtin`, …). A **claim** names one
or more **slots**, satisfied atomically or not at all, possibly from benches on
different hosts; among adequate benches the matcher picks the *least capable* one,
scored by the scarcity it would waste. A granted claim is a **lease** with a mandatory
explicit TTL, renewable only by explicit call. Executors fence on a per-bench epoch,
so a stale instruction can never hand out live hardware; and because every lease has a
deadline, a network partition drains the lab rather than corrupting it.

## Layout

```
docs/design.md          the design, 20 decisions, and open questions
src/tags.rs             key=value vocabulary, validation, implication closure
src/model.rs            benches, resources, claim requests, TOML config
src/matcher.rs          matching, best-fit scoring, allocation, diagnosis
tests/matcher.rs        behavioural spec + property test vs a brute-force oracle
examples/inventory.toml example inventory
```

The crate will split into a workspace (`benchd-core`, `-proto`, `-coordinator`,
`-host`, `-client`) when the daemons land; today's code is all pure core.

## Build

```sh
cargo test           # 19 tests, incl. a property test over random inventories
cargo clippy --all-targets
```

Requires Rust 1.88+ (MSRV is pinned by `rmcp`, used once the MCP server lands).

## Reading order

`tests/matcher.rs` is written to be read as the specification — test names and
assertions state what the matcher promises. Start there, then `src/matcher.rs` for
the only module with real algorithmic content.

## Design in one paragraph

A **bench** is the unit of exclusion: a named set of resources held together. Benches
carry `key=value` capability tags from a closed vocabulary, expanded through an
implication graph (`soc=esp32s3` implies `family=esp32`, `jtag=builtin`, …). A
**claim** names one or more **slots** and is satisfied atomically or not at all;
among adequate benches the matcher picks the *least capable* one, scored by the
scarcity it would waste. A granted claim is a **lease** with a mandatory explicit
TTL, renewable by explicit call, reaped in-process. The lease materialises real
device inodes via bind mount into a per-lease directory, and revocation removes
them — so the lease is enforced by the kernel rather than by convention.
