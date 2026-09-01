# benchd — design

**Status:** design settled, partially implemented. Matcher done and tested; the three
daemons are specified here but not written.

**This document is authoritative.** When it and the code disagree, fix one of them —
don't leave them in conflict.

---

## 1. Problem

One machine, many ESP32 boards, several agents working concurrently. Agent A starts a
test; agent B flashes the same board mid-run. Both results are garbage and nothing
reports an error.

Agents need exclusive, time-bounded custody of hardware, and that custody must be
**real** rather than advisory.

## 2. Goals

1. **Claim by capability, not by name** — `claim(soc=esp32s3)`. Re-cabling the lab
   must not break agent scripts.
2. **Normal tools work.** After a claim, `esptool`, `idf.py monitor`, `minicom` and
   `openocd` operate on a real character device. No wrapper, no URL scheme.
3. **Enforced, not advisory.** Without a lease the device is unreachable. "Forgot to
   claim" is `ENOENT`, not silent corruption of someone else's run.
4. **Claims expire.** Mandatory explicit TTL. Agents crash, wander off, and hoard.
5. **Failures are self-diagnosable.** "No bench will ever match this" and "they're all
   busy" are structurally different answers.

## 3. Non-goals

- **Authenticating clients.** Client identity is self-asserted — policy, not security
  (§9). Host identity *is* authenticated.
- **Partition tolerance.** The coordinator is a SPOF by choice (D6).
- **A test framework.** benchd hands out hardware; what you do with it is your business.
- **Driving hardware for the agent.** No console/flash/power tools in the agent
  surface (D17).

---

## 4. Concepts

| Concept | Definition |
|---|---|
| **Resource** | One physical thing: a serial device (by `/dev/serial/by-id` path), a USB device, a relay, a probe. |
| **Bench** | Unit of exclusion *and* ownership. A named set of resources — possibly several boards — always held together. Lives on exactly one host. |
| **Tag** | A `key=value` capability fact about a bench. |
| **Slot** | A named role in a claim (`dut`, `peer`), filled by one whole bench. |
| **Claim** | A request for one or more slots, satisfied atomically or not at all. Slots may come from different hosts. |
| **Lease** | A granted claim: which benches, held by whom, until when. Carries a per-bench **epoch** (D7). |
| **Host** | Owns the hardware of one bench; executes instructions. Many per machine. |
| **Coordinator** | Sole authority for inventory, matching, policy and lease state. |
| **Client** | Privileged daemon on an agent machine: serves MCP, materialises device nodes. |

**Multi-resource bench vs. multi-slot claim** — these look alike and are not:

| | Multi-resource bench | Multi-slot claim |
|---|---|---|
| Boards are | wired together, or share a rail/chamber/hub | independent |
| Grouped by | the operator, in config | the agent, per claim |
| Separable? | never | yes |
| Example | a mesh carrier; a DUT plus its RF chamber | "any S3 plus another ESP32 to talk to" |

If separating them would be *physically meaningless*, it's one bench.

---

## 5. Architecture

```
  agent (pi / claude / cursor)
      │ MCP over stdio
      ▼
 ┌──────────────────────────┐        ┌───────────────────────────────┐
 │ CLIENT  (agent machine)  │        │ COORDINATOR   (only listener) │
 │  · MCP server            │───────▶│  · inventory + matcher        │
 │  · sandbox materialiser  │  gRPC  │  · policy + lease state       │
 │  · runs as root          │  mTLS  │  · reaper                     │
 └──────────┬───────────────┘        └───────────────▲───────────────┘
            │ bind mount                             │ gRPC (mTLS)
            ▼                                        │
 /run/benchd/<owner>/<lease>/<slot>/<res>    ┌───────┴──────────┐
                                             │ HOST (one bench) │ ×N
                                             │  · owns the HW   │
                                             │  · power, mux    │
                                             └────────┬─────────┘
                                                      ▼
                                              /dev/ttyACM0, relay, …
```

Arrows show who dials: **both executors dial in, the coordinator never dials out**
(D5).

**One host process per bench.** Blast radius of a wedged host is one bench; each
restarts independently; device ownership is unambiguous. Deployed as
`benchd-host@mesh-rig.service`, so N benches is a config concern.

**Same-machine is not a special case.** Co-located, the client and host are just two
processes on one box and the materialiser bind-mounts a local inode. The protocol is
identical — which is the point of doing this now rather than retrofitting it (D4).

---

## 6. Decisions

### D1. Leases, enforced — not jobs, not advisory locks

A lease is mutual exclusion over a bounded window, held across many commands. Without
one, the device node does not exist.

*Why not CI jobs:* hardware work is iterative — flash, poke, read, tweak. Wrapping each
step in a job submission turns a seconds-long loop into minutes, and agents compensate
badly. It also doesn't solve the problem: unless *all* access goes through it, a shell
still owns `/dev/ttyUSB0`.

*Why not advisory locks:* they work on humans because social pressure covers the gap.
Agents have none — one that forgets, or saw `--port /dev/ttyUSB0` in a README, or
decides a lock is "probably stale", corrupts someone else's run silently.

### D2. Real device nodes, at per-lease paths

Bind-mount the real device inode to
`/run/benchd/<owner>/<lease-id>/<slot>/<resource>`, exposed as `$LAB_DUT_CONSOLE`.

*Why bind mounts:* Goal 2 rules out everything else. `rfc2217://` and `socket://` are
pyserial-only — `idf.py monitor`, `minicom` and `openocd` refuse them. A pty bridge
loses modem-control lines, breaking ESP32 auto-reset intermittently. Symlinks dangle in
a sandbox with no `/dev`. A bind mount behaves exactly like the device because it *is*
the device.

*Why the lease id is in the path:* if `/dev/lab/dut` meant board A last lease and board
B this lease, a stale shell writes to the wrong board — the original failure,
reintroduced. Stale paths must fail `ENOENT`.

*Rejected:* literal `/dev/ttyUSB0`. Kernel indices renumber and collide, reintroducing
the identity problem tags exist to remove.

### D3. Greenfield, not labgrid

*Why:* the piece we need most — a device node materialising in a client-side sandbox —
fits labgrid's architecture worst. Its model is "resource stays on the exporter, client
talks over the network through a Driver"; a root-side inode bind-mounter is none of
Resource/Driver/Exporter. The parts we'd have to modify are its highest-coupling area
(`remote/`, 6.5k LOC), to inherit 9.6k LOC of drivers we need none of.

*The dilemma dissolves* because labgrid's drivers don't depend on its coordinator
(verified — appendix). When a PDU or SD-mux arrives, `pip install labgrid` and use the
driver against a locally-declared resource. No fork, no coordinator.

*Risk:* at six machines with PDUs we'll have rebuilt part of labgrid, worse. Mitigated
by keeping `Resource` an interface and matcher/policy/leases ignorant of transport.

### D4. Three components, distributed from the start

Coordinator, host, client. A single-machine lab runs all three locally over the same
protocol.

*Why not local-first-behind-a-trait:* distribution is not a backend detail — it changes
*where authority lives*. Local means one process holding a lock; distributed means
executors that can act on stale instructions, needing fencing (D7) and a failure story
(D6). Retrofitting that rewrites the lease manager rather than swapping a trait.

*What stays a trait:* the materialiser, because the delivery mechanism genuinely varies
— bind mount when co-located, USB/IP when not.

### D5. Only the coordinator is reachable; gRPC streams both ways

The coordinator is the sole listener. Hosts and clients always dial in and hold a
long-lived bidirectional stream; instructions travel back down it.

```proto
rpc HostStream(stream HostMessage)     returns (stream CoordinatorMessage);
rpc ClientStream(stream ClientMessage) returns (stream CoordinatorMessage);
```

*Why:* hosts live wherever the hardware is — lab VLAN, bench laptop, behind NAT.
Requiring each to be addressable makes adding a bench an infrastructure task instead of
a `systemctl start`, which is the tax that makes people give up and plug everything
into one box. It also reduces the attack surface to one port and simplifies certs: a
server cert for the coordinator, client certs for everyone else.

*Why streams, specifically:* the coordinator must reach executors it cannot dial, so
server-initiated messages over an inbound stream are the only delivery mechanism
available — not a convenience. The same stream carries heartbeats up and `revoking`
notifications down.

*Cost, accepted:* a stream is a message pipe, not RPC. Coordinator→executor calls need
an application-level `request_id`, correlation and timeouts, which gRPC would have given
free in the other direction.

**USB/IP runs the wrong way.** Per the kernel README, the "server" is the machine *with*
the device — our host — listening on 3240, and the client dials in and holds that
connection for the whole attachment. A direct remote attach therefore needs exactly the
host reachability this decision refuses, so **remote device traffic is relayed through
the coordinator**. That would have been unacceptable under a partition-tolerant design;
under D6 it costs nothing, because coordinator death is already fatal. Costs: an extra
hop on a latency-sensitive path (fine for 115200 serial, unproven for OpenOCD), and the
coordinator as a bandwidth bottleneck. Co-located benches are unaffected — bind mount,
no network.

### D6. The coordinator is a SPOF; any restart releases leases

All lease state lives in the coordinator and nowhere else. Restarting any component
releases the leases it owns; every component tears down what it owns at startup, before
accepting work. If the coordinator is unreachable for longer than a short grace period,
executors release and the lab drains.

*Why:* partition tolerance costs replicated or reconstructable state, a way to tell a
partition from a restart, and a split-brain story — a distributed state machine, for a
lab whose leases last minutes and whose agents can simply re-claim. Persistence costs a
database, a reconcile protocol, and bugs where recorded intent and physical reality
disagree.

*What it buys:* the coordinator is **stateless** — no sqlite, no schema, no reconcile.
The restart hole (kernel state outliving every process, leaving hardware reachable by an
agent whose lease is gone) closes by construction: at startup nothing is mounted,
because each component just removed it.

*Grace, and why it isn't zero:* a two-second blip shouldn't destroy everyone's work.
Executors release after `G` seconds without the coordinator. On reconnect inside `G`
they declare what they still hold and the coordinator confirms or denies each lease — a
restarted coordinator denies everything, being stateless, so one mechanism covers blip
and restart with no incarnation ids or restart-detection.

*Accepted:* coordinator availability is lab availability. Run it on the box that's
already always on.

### D7. Executors fence on a lease epoch

Every grant carries a monotonically increasing per-bench `epoch`. Hosts and clients
record the highest seen and reject anything lower.

*Why:* this replaces the in-process lock a single binary got for free. Executors across
a network can act on stale instructions — the classic failure is a delayed
`materialize` for lease *N* arriving after *N* expired and *N+1* was granted to someone
else, handing live hardware to an agent whose lease is gone, silently.

*Not timestamps:* clock skew is exactly what you can't assume away, and it fails
silently.

*Consequence:* all executor operations are idempotent and epoch-qualified.
`unmaterialize` of an unknown lease is a no-op — the reaper races voluntary releases and
neither path may fail.

### D8. The client is a privileged daemon, not a library

One root daemon per agent machine, serving MCP *and* materialising.

*Why privileged:* the node must appear on the agent's machine (Goal 2). Bind-mounting
needs `CAP_SYS_ADMIN`; `usbip attach` needs root. The agent stays unprivileged, so
something local holds privilege for it.

*Why one daemon:* an unprivileged shim plus a privileged helper needs its own IPC and
authorisation, guarding against an attacker already out of scope (§9).

### D9. Central vocabulary, host-declared benches

Each host declares its own bench — resources, tags, description — and registers it. The
**tag vocabulary lives with the coordinator**, and registrations using unknown tags are
refused.

*Why host-declared:* config belongs next to the hardware. Adding a board is a
single-machine operation with no central file to keep in sync.

*Why central vocabulary:* if hosts also defined tags, the closed vocabulary stops being
closed and rots into `esp32-s3`/`esp32s3`/`s3` per machine — the exact failure D10
prevents, with an extra dimension to rot along. *What exists* is local; *what things are
called* is global.

### D10. Tag vocabulary is closed, `key=value`, with implications

Benches expand through an implication graph (`soc=esp32s3` ⇒ `family=esp32`,
`jtag=builtin`, …); unknown tags are rejected with did-you-mean suggestions.

*Why:* free-form tags rot within a week once *agents* write the requests. A closed
vocabulary turns a typo into a self-correcting error in one turn instead of ten turns of
"no bench matches".

*Deliberate asymmetry:* bench tags are expanded, requirements never are — expanding a
request makes it strictly harder to satisfy.

*Dashes:* legal in values generally (`name=esp32s3-a` is an opaque identity string),
rejected in *vocabulary* values, which is where the `esp32-s3`/`esp32s3` split happens.

### D11. A bench may hold several boards

A bench's resource map is arbitrary in size; claiming it materialises all of it.

*Why:* a mesh rig with three ESP32s on a carrier sharing a power rail can't be
meaningfully split. Making the bench the unit of exclusion *at whatever size the
hardware actually is* keeps the guarantee honest.

*Rejected:* three benches plus an affinity constraint — it would have to be satisfied
atomically anyway, and it would let an agent request two of the three nodes, which is
the meaningless request we want to be unable to express.

*Consequence:* tags describe the whole bench (`nodes=3`), never an individual board. If
boards inside a bench differ in ways agents must select on, they should have been
separate benches.

### D12. Best fit, weighted by scarcity

Among adequate benches pick the least capable: cost is `Σ weight(key) / (benches with
that tag)` over wasted tags.

*Why:* first-fit hands the only JTAG bench to an agent that asked for a blinking LED.

*Why weighted — a bug we hit:* pure scarcity conflates *rare* with *valuable*. Being the
only CP2102N made `usb=cp2102n` unique, so the matcher protected the lab's cheapest
board and handed out the PSRAM one. Identity keys (`soc`, `arch`, `usb`) get weight 0;
contended peripherals keep 1.

### D13. Atomic multi-slot claims, solved exactly

All slots or none; assignment is exact (branch-and-bound), not greedy.

*Why atomic:* two agents each holding half of what they need is a deadlock.
*Why exact:* greedy can report failure for a satisfiable request, and a false "no bench
available" is indistinguishable from real contention — agents would wait forever.

### D14. Unsatisfiable vs contended is a type, not a string

`Failure::Unsatisfiable | Failure::Contended`, plus near-miss reporting.

*Why:* they demand opposite behaviour — change the request versus wait. Collapsed into
"no bench available", agents retry-spin on impossible requests forever.

```
slot 'dut': no bench exists matching {psram=octal soc=esp32c3}
  no bench has: soc=esp32c3
  drop soc=esp32c3 -> matches {psram=octal}
```

### D15. Mandatory explicit TTL; renewal is an explicit call

No default duration. `max_ttl` bounds one grant, `max_total_hold` bounds the sum across
renewals, `max_benches` bounds concurrency.

*Why explicit:* naming a duration forces the agent to scope the work, and gives
requested-vs-used telemetry to tune limits from data.

*Why not auto-keepalive:* it recreates the never-expiring hold we're eliminating. An
explicit renew is a liveness proof — alive *and* still working.

### D16. Human preemption; agents never preempt

*Why:* this is the real payoff of the human/agent split, not longer timeouts. When
you're at the bench and an agent holds the board, you take it back.

*Grace:* preemption and expiry both mark the lease `revoking` and wait ~30s before
teardown. Yanking a device mid-flash can leave a board in bootloader.

### D17. Minimal agent tool surface

`tag_list`, `claim`, `renew`, `release`, `lease_status`. Nothing else.

*Why:* tool surface *is* policy. A console-read tool means agents use it instead of
`idf.py monitor`, giving two access paths and split logs. Claim-by-name means an agent
hardcodes a bench into a script, reintroducing the contention this removes. Both stay in
the human CLI.

### D18. Rust, TOML, workspace

*Why Rust:* a root daemon calling `mount(2)` on paths derived from agent-supplied
strings is the worst place for a memory-safety bug; and correctness rests on
single-writer lease state, which Rust makes a compile-time property. The domain is
algebraic (`Held | Revoking | Expired`), so sum types model it natively.

*Rejected:* C++ (dependency management; the safety argument bites hardest here); Go
(viable — better ergonomics, `syscall.Mount` in stdlib — but no sum types and races drop
to `go test -race`); Haskell (fits beautifully, but no MCP SDK, thin udev ecosystem, and
it blunts the agent-assisted debugging this project relies on).

*TOML because* `serde_yaml` is `0.9.34+deprecated`. *Verified:* `rmcp` 3.2.0, MSRV 1.88,
matching the installed toolchain.

```
benchd-core         tags, model, matcher, policy   pure, no I/O, property-tested
benchd-proto        .proto + generated tonic stubs
benchd-coordinator  matching, policy, lease state, reaper   (stateless)
benchd-host         owns one bench; export/teardown; power/mux
benchd-client       MCP server + sandbox materialiser (root)
```

Core stays free of tonic/tokio/rustls so its property test stays millisecond-scale.

---

## 7. Components

**Coordinator** — sole writer of lease state, stateless across restarts. Holds the
vocabulary and validates host registrations against it; tracks host liveness by
heartbeat; serves `claim`/`renew`/`release`/`lease_status`/`tag_list`; allocates epochs;
runs the reaper (grace → revoke → teardown).

> **Ordering rule:** export on the host *before* materialising on the client; tear down
> on the client *before* unexporting on the host. A client must never hold a node the
> host believes is free.

**Host** — owns one bench, decides nothing, executes epoch-qualified instructions.
Registers its bench upward; `export` / `unexport`; heartbeats; owns power/mux for setup
and teardown only. Unexports everything at startup. On device loss it releases, reports,
and exits for a clean systemd restart.

**Client** — one privileged daemon per agent machine. Serves MCP with per-agent identity
per request; materialises and revokes; renews only on explicit agent call. Removes every
materialisation under its root at startup.

> **Sandbox mechanism: bubblewrap.** `/run/benchd/<owner>` is bind-mounted into the
> agent's sandbox at start. Because it's a *directory* mount, entries appear and
> disappear inside a running sandbox with no restart and no cooperation from the agent.
> Chosen as the easiest start; the materialiser is a trait, so Docker or ACLs can follow.

**Matcher** *(implemented)* — `allocate(request, benches, busy, counts, weights)`.
Invariants pinned by property test: succeeds ⟺ a valid assignment exists; returned cost
is optimal; returned assignment is valid.

**Policy** *(specified)* — identity → class → limits. Unknown identities fall back to
the *most restricted* class, so a typo can never grant human privileges. Over-long TTL
is **clamped with a message**, not rejected; other limits are hard errors that say what
to do instead.

```
agent:  ttl≤15m   total≤2h    benches≤2   renewable
human:  ttl≤8h    total ∞     benches ∞   renewable, may preempt, may claim by name
ci:     ttl≤45m   total≤45m   benches≤4   NOT renewable (fail fast)
```

**Lease lifecycle** — `HELD ⇄ renew`; `HELD → REVOKING` on grace or preempt;
`REVOKING → EXPIRED | RELEASED`. Every tool response carries `expires_at` and
`remaining`; agents plan terribly against invisible deadlines.

---

## 8. What agents see when things go wrong

A contract; the skill must state it.

| Situation | Agent sees | Correct response |
|---|---|---|
| Unknown tag | `unknown tag soc=esp32s4 (did you mean: soc=esp32s3?)` | fix, retry immediately |
| No such bench | `Unsatisfiable` + which tags to drop | change the request; never retry as-is |
| All busy | `Contended` + holders + ETA | wait, retry |
| Over TTL limit | clamped, with a message | proceed with the shorter lease |
| Lease expired mid-use | `ENOENT` / `EIO` on the device | **your lease ended** — reclaim; do *not* power-cycle |
| A component restarted | lease gone, `lease_released` on next call | re-claim; work since last checkpoint is lost |
| Bench hardware failed | lease released, bench leaves `tag_list` | re-claim; the matcher routes around it |

The `ENOENT` row matters most: without it agents read revocation as broken hardware and
start power-cycling boards to fix a timeout. The last two mean a lost lease is routine,
not an error to escalate.

---

## 9. Security posture

**Client identity is self-asserted — policy, not security.** An agent claiming to be
`tom` gets human limits. This stops honest mistakes and runaway agents, nothing that is
trying. If it ever must be a boundary, it goes in the transport, not the broker.

**Host identity is authenticated.** A rogue host can advertise benches that don't exist,
absorb claims, and mislead an agent about which board it's driving. Coordinator↔host
uses mTLS with pinned certs; unknown hosts are refused. A lying client harms itself; a
lying host harms everyone.

**USB/IP is never exposed** — cleartext, unauthenticated, and it hands the client kernel
a USB device. It is relayed through the coordinator (D5), never reachable directly.

Hosts and clients run as root but only execute epoch-qualified instructions from an
authenticated coordinator, never agent-supplied strings, and build paths only from
validated identifiers.

---

## 10. Open questions

| # | Question | State |
|---|---|---|
| Q10 | Relay implementation for remote USB/IP: framing, flow control, backpressure — and does OpenOCD tolerate the extra hop? | open; only bites when remote benches land |

**Deferred ideas.** *Idle release* — TTL catches crashes, not an agent that claims a
board then reads source for 12 minutes; detect via no open fd for N minutes. *Sticky
reclaim* — a short soft-reservation after release so flash→test→tweak stays on one
board. *Bench affinity* — for two benches sharing an RF chamber. All revisit-with-
evidence, not now.

---

## 11. Status

| Component | State |
|---|---|
| Tags, vocabulary, implications | done, tested |
| Bench/resource model, TOML config | done, tested (schema splits per D9) |
| Matcher | done — 21 tests + property test |
| Multi-resource benches | done, tested |
| Policy engine | Python prototype only |
| Workspace split | not started |
| Wire protocol / `.proto` | not started |
| Coordinator / host / client | specified |
| USB/IP relay | specified, not started |
| Skill | not started |

No persistence layer appears here, and that is the point of D6.

**Order:** workspace split → policy → `.proto` → coordinator lease manager → host →
client materialiser → MCP → skill.

---

## Appendix A — labgrid findings (v26.0, checked 2026-08)

Kept because D3 rests on them.

- `remote/scheduler.py` matches with `f.tags.issubset(place.tags)` — the same semantic
  we want, with a contention-aware allocator.
- Acquired places **never expire**; `coordinator.py` carries a `FIXME` asking for
  exactly that.
- Multi-group reservations exist in the scheduler, but `client.py:1581` *and*
  `coordinator.py:1031` both hardcode `filters["main"]`.
- **No USB/IP support** anywhere in code, docs or issues. Serial export is ser2net
  RFC2217 (`exporter.py:260`), consumed as `rfc2217://…?ign_set_control`
  (`serialdriver.py:47`).
- Drivers do **not** depend on the coordinator: `driver/powerdriver.py` imports only
  `..resource`, `..protocol`, `..step`, `..util`. This is what makes labgrid usable as a
  plain library later.
- Sizes: 25.7k LOC total — `driver/` 9.6k, `remote/` 6.5k, `resource/` 3.0k.

## Appendix B — corrections made during design

Both were caught by checking, not by thinking harder.

1. **"Multi-slot claims are reachable through labgrid's gRPC API without patching."**
   Wrong — the coordinator's own scheduling loop hardcodes `filters["main"]`, so it
   would have needed a coordinator patch.
2. **Unweighted scarcity scoring** would have made the matcher hoard the lab's cheapest
   board, because rarity stood in for value. Caught by running it against the real
   inventory.
