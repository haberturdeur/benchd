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
 │ benchd-mcp   (per agent) │        │ COORDINATOR   (only listener) │
 │  · thin, unprivileged    │        │  · inventory + matcher        │
 └──────────┬───────────────┘        │  · limits + lease state       │
            │ local socket           │  · reaper                     │
 ┌──────────▼───────────────┐  JSON  │                               │
 │ benchd-clientd (machine) │───────▶│                               │
 │  · sandbox materialiser  │  lines │                               │
 │  · runs as root          │  / TCP │                               │
 └──────────┬───────────────┘        └───────────────▲───────────────┘
            │ bind mount                             │ JSON lines / TCP
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
(D5) — including for remote USB/IP, which is relayed rather than direct, so a NAT'd
host works with no inbound reachability at all.

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

### D5. Only the coordinator listens; newline-delimited JSON over plain TCP

The coordinator is the sole listener. Hosts and clients dial in and hold the connection
open; instructions travel back down it. Messages are `serde` enums in a shared crate,
one JSON object per line, over **plain TCP everywhere** — no unix-socket special case
for co-located components, no TLS.

*Why coordinator-only:* hosts live wherever the hardware is — lab VLAN, bench laptop,
behind NAT. Requiring each to be addressable makes adding a bench an infrastructure
task instead of a `systemctl start`.

*Why a held-open connection:* the coordinator must reach executors it cannot dial, so
pushing down an inbound connection is the only delivery mechanism available — not a
convenience. The same connection carries heartbeats up and `revoking` down.

*Why no TLS:* **benchd does no cryptography.** The PoC assumes a trusted LAN (§9).
Rolling a PKI is a notorious time sink, and the half that would matter — issuing
*client* certs so hosts can be identified — is the half nothing automates. If this ever
needs a boundary it goes underneath as WireGuard, whose peer public keys are already
mutual authentication, rather than into the application.

*Rejected — a TLS-terminating proxy such as Caddy:* raw TCP would need `caddy-l4`, an
experimental non-official plugin requiring an `xcaddy` custom build — and worse, **raw
TCP through a terminating proxy loses the client identity**. HTTP can carry the
verified subject in a header; a byte stream cannot (PROXY protocol carries the source
address, not TLS identity). Using Caddy for auth would mean speaking HTTP/WebSocket and
putting a proxy in the hot path, to solve a problem better solved one layer down.

*Rejected — gRPC/protobuf:* its advantages were schema codegen and cross-version
compatibility, and multi-version operation is an explicit non-goal. All binaries build
from one workspace, so sharing Rust types directly is stronger sync than generating
them, at no cost in `build.rs`, `prost` or a second language. What we hand-roll instead
is small and dull: newline framing (`LinesCodec`), a `request_id` for correlation, a
heartbeat for liveness, a reconnect loop.

*Bonus, and it matters for a PoC:* the wire is human-readable. `socat` is a debugger.

**USB/IP runs the wrong way, so we hand the kernel our own socket.** Per the kernel
README the machine *with* the device listens on 3240 and the client dials in — the
opposite direction from our control plane, and impossible when the host is behind NAT.
But nothing about USB/IP actually requires a listening socket. Both kernel drivers take
an **already-connected fd**:

```c
/* stub_dev.c  — host side  */  usbip_sockfd_store():  sockfd_lookup(sockfd); SOCK_STREAM?
/* vhci_sysfs.c — client side */  attach_store(): "port sockfd devid speed"
                                  /* @sockfd: socket descriptor of an established
                                     TCP connection */
```

So neither end listens. Both dial **out** to the coordinator, which splices the two
sockets; each side then performs its half of the USB/IP handshake on that socket and
hands the fd to the kernel:

```
1.  coord → host    Export{lease,epoch}         host: bind device to usbip-host
2.  coord → host    OpenChannel{lease,epoch,K}  host dials OUT, hello{K}
3.  coord → client  Materialize{lease,epoch,K}  client dials OUT, hello{K}
4.  coordinator      splices the two sockets matching K (copy_bidirectional)
5.  host             OP_REQ_IMPORT/OP_REP_IMPORT handshake, then
                     echo $fd > .../usbip_sockfd
6.  client           same handshake, then
                     echo "$port $fd $devid $speed" > .../vhci_hcd.0/attach
7.  kernel           stub_rx/tx and vhci_rx/tx own the socket; both daemons leave the
                     data path entirely
8.  client           bind-mount the resulting node into the agent's lease directory
```

*What this deletes:* no `usbipd` process, no listening socket on the host, no wildcard
bind, no firewall rule, no loopback listener or port allocation on the client, and no
`usbip --tcp-port` juggling. D5's "nothing listens except the coordinator" becomes
*literally* true rather than true-modulo-a-firewall.

*What it costs:* implementing the handshake — `OP_REQ_IMPORT` / `OP_REP_IMPORT`, a
header plus `struct usbip_usb_device`, fixed-layout big-endian. Perhaps 200 lines. The
layout must be exact, but it is **differentially testable**: our host against stock
`usbip attach`, our client against stock `usbipd`.

*Teardown:* writing `-1` to `usbip_sockfd` tears the stub down; the host does that for
every bound device at startup, which is how D6's "unexport everything before accepting
work" is implemented. The kernel takes its own reference via `sockfd_lookup`, so the
socket survives the daemon that created it — exactly why an explicit teardown is
required rather than optional.

*Costs of the relay itself, accepted:* the coordinator sits in the data path, adding
bandwidth load and one RTT per URB round trip. Irrelevant for the serial consoles this
is built for (~11 KB/s). JTAG and other latency-sensitive transports are out of scope
(Q10). Coordinator death already kills the lab (D6), so this costs no availability that
was not already gone.

*Deliberately not done:* having the host dial the client directly when the client is
reachable. It would cut the coordinator out of the data path, but it is a second code
path for a topology we cannot rely on. Revisit if the relay measures badly.

*Not used locally.* Co-located benches never touch USB/IP: the daemon bind-mounts the
real inode. The control protocol is identical either way; only the `Materializer`
implementation differs, which is what the trait is for (D4).

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

### D8. A privileged daemon per machine, a thin MCP shim per agent

`benchd-clientd` is one root daemon per agent machine, holding the coordinator
connection and doing all materialisation. `benchd-mcp` is a tiny unprivileged MCP
server, spawned per agent by its harness, talking to the daemon over a local socket.

*Why the daemon is privileged:* the device node must appear on the agent's machine
(Goal 2). Bind-mounting needs `CAP_SYS_ADMIN`; `usbip attach` needs root. The agent
stays unprivileged, so something local holds privilege for it.

*Why the split is not optional:* **MCP over stdio is one process per client** — stdio
is a pipe pair, so the harness spawns the server. A single daemon cannot serve stdio
MCP to several agents. (An earlier draft of this decision said "one daemon, no shim";
that was simply wrong about how stdio works.) The shim stays thin and unprivileged;
privilege lives in the daemon.

*Rejected:* serving MCP over local HTTP so one daemon handles every agent. It works
(`rmcp` supports it) but pushes per-agent identity into a header the harness must set,
which is more fragile than a process boundary that already exists.

### D19. Identity is a session token; the name is only a label

At startup `benchd-mcp` registers with the coordinator, declaring a name from
`$BENCHD_IDENTITY` in its sandbox, and receives a **session token** (UUID) carried on
every later call. **There is no authentication** — registration is a request for a
token and the coordinator always grants one.

*The name is diagnostic, not authoritative.* With no roles (D15) it grants nothing; it
exists so `lease_status` and contention reports can say *"held by agent-3, expires in
240s"* instead of quoting a UUID at you. Nothing branches on it.

*Why nothing is stored:* the token is issued at boot, lives in memory, and dies with
the process. No file under `/run`, no state to reconcile, no stale identity to clean
up. This falls out of D6: if a restart releases leases, identity that outlives a
restart has nothing left to be useful for.

*What the session buys:* it is the natural owner of a lease and the natural unit of
liveness. When the MCP process exits its socket closes, the daemon tells the
coordinator, and that session's leases are released **immediately** rather than waiting
out their TTL. TTL becomes the backstop for a wedged-but-alive agent rather than the
only reclaim path.

*Accounting is per session:* a restarted agent gets a fresh session and a fresh
`max_benches` budget. Under §9's no-malice assumption that is fine.

*The seam is deliberate:* `Register { name } -> SessionToken` becomes
`Register { name, credential } -> SessionToken` if authentication is ever wanted — one
function, not an architecture.

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

### D15. Mandatory explicit TTL; renewal is an explicit call; one set of limits

No default duration. One global limit set applies to every session — there are **no
user classes and no roles**.

```toml
[limits]
max_ttl        = "15m"   # longest single grant
max_total_hold = "2h"     # sum across renewals, so nobody renews forever
max_benches    = 2        # concurrent benches per session
```

*Why explicit TTL:* naming a duration forces the agent to scope the work, and gives
requested-vs-used telemetry to tune the limits from data rather than guesswork.

*Why not auto-keepalive:* it recreates the never-expiring hold we're eliminating. An
explicit renew is a liveness proof — alive *and* still working.

*Why no classes:* an earlier draft had agent/human/ci tiers with different ceilings and
rights. That is a permission system, and a permission system needs identity to mean
something — which §9 says it does not. One set of numbers is honest about what we can
actually enforce, and the operator escape hatch (D16) covers what the `human` tier was
really for.

*Over-long requests are clamped with a message*, not rejected — an agent asking for 4h
and getting 15m can get on with its work. Other limits are hard errors that say what
to do instead.

### D16. No preemption between sessions; the operator has a CLI

No session can take a bench from another. The operator can, with
`benchd release --bench <id> --force`, which talks to the coordinator directly.

*Why:* "you're at the bench and an agent is holding the board" is a real need, but it
is an *administrative action*, not a role in a permission model. Making it a CLI
command keeps the need met and deletes the tier system that existed to express it.

*Grace:* forced release and ordinary expiry both mark the lease `revoking` and wait
~30s before teardown. Yanking a device mid-flash can leave a board in bootloader.

### D17. Minimal agent tool surface

`tag_list`, `claim`, `renew`, `release`, `lease_status`. Nothing else.

*Why:* tool surface *is* policy — and with roles gone it is the *only* policy lever
left, which makes it more load-bearing, not less. A console-read tool means agents use
it instead of `idf.py monitor`, giving two access paths and split logs. Claim-by-name
means an agent hardcodes a bench into a script, reintroducing the contention this
removes.

*The distinction is the surface, not the caller:* both live in the operator CLI, which
is a different program — not a privileged mode of the same one.

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
benchd-core         tags, model, matcher, policy, wire messages   pure, no I/O
benchd-coordinator  matching, policy, lease state, reaper   (stateless)
benchd-host         owns one bench; export/teardown; power/mux
benchd-clientd      privileged: coordinator link + sandbox materialiser
benchd-mcp          thin per-agent stdio MCP shim (unprivileged)
benchd              operator CLI
```

With no `.proto` (D5) the wire messages are just `serde` types and live in core, which
already depends on serde for config. Core stays free of tokio so its property test stays
millisecond-scale. `benchd-mcp` and `benchd` are small enough to be binaries in the
client and coordinator crates rather than crates of their own.

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

**Client** — `benchd-clientd`, one privileged daemon per agent machine, holding the
coordinator connection. Materialises and revokes; renews only on explicit agent call;
removes every materialisation under its root at startup. `benchd-mcp` is the thin
unprivileged stdio shim spawned per agent (D8), which registers a session and forwards
calls over a local socket.

> **Sandbox mechanism: bubblewrap.** `/run/benchd/<owner>` is bind-mounted into the
> agent's sandbox at start. Because it's a *directory* mount, entries appear and
> disappear inside a running sandbox with no restart and no cooperation from the agent.
> Chosen as the easiest start; the materialiser is a trait, so Docker or ACLs can follow.

**Matcher** *(implemented)* — `allocate(request, benches, busy, counts, weights)`.
Invariants pinned by property test: succeeds ⟺ a valid assignment exists; returned cost
is optimal; returned assignment is valid.

**Policy** *(specified)* — one global limit set, applied to every session; no classes,
no roles (D15). Over-long TTL is **clamped with a message**, not rejected; other limits
are hard errors that say what to do instead.

```toml
[limits]
max_ttl        = "15m"
max_total_hold = "2h"
max_benches    = 2
```

**Operator CLI** — `benchd` talks to the coordinator directly and is not subject to the
limits: list benches by name, inspect leases, `release --force` (D16). Deliberately a
separate program from the agent surface, not a privileged mode of it.

**Lease lifecycle** — `HELD ⇄ renew`; `HELD → REVOKING` on grace, forced release, or
session loss; `REVOKING → EXPIRED | RELEASED`. Every tool response carries `expires_at`
and `remaining`; agents plan terribly against invisible deadlines.

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
| Operator forced a release | lease `revoking`, then gone | stop, park the board, re-claim later |
| A component restarted | lease gone, `lease_released` on next call | re-claim; work since last checkpoint is lost |
| Bench hardware failed | lease released, bench leaves `tag_list` | re-claim; the matcher routes around it |

The `ENOENT` row matters most: without it agents read revocation as broken hardware and
start power-cycling boards to fix a timeout. The last two mean a lost lease is routine,
not an error to escalate.

---

## 9. Security posture

**There is none, and that is deliberate.** The PoC assumes a trusted LAN with no
malicious hosts or clients. Anyone who can reach the coordinator's port can register a
session, claim hardware, and register a bench.

What this buys: no PKI, no cert distribution or rotation, no auth code in the hot path,
and no security theatre implying a boundary that isn't there. What it costs: benchd
**must not** be run on a network you don't control.

There are no roles (D15), so identity grants nothing — the session name is a label for
diagnostics, not an authorisation input (D19). Limits are global and apply to everyone
equally; they exist to stop a runaway agent hoarding boards, not to stop an attacker.

If this ever needs a real boundary, the seams are deliberate and in this order:

1. **WireGuard underneath** — peer public keys are already mutual authentication, and
   benchd still does no cryptography.
2. **`Register { name, credential }`** — the registration call already has the shape
   (D19); adding a check is one function.
3. **Host authentication** matters more than client authentication — a lying client
   harms only itself, a lying host can advertise benches that don't exist and mislead
   an agent about which board it is driving.

Hosts and clients run as root but only execute epoch-qualified instructions from the
coordinator, never agent-supplied strings, and build paths only from validated
identifiers. That is defence against *bugs*, which remains worthwhile regardless.

**We do not run `usbipd`, and that is partly a security decision.** It cannot be
confined to an interface (verified in `usbipd.c`: `do_getaddrinfo(NULL, family)` with
`AI_PASSIVE` binds wildcard; there is no bind-address option), and its protocol has no
authentication — `recv_request_import` matches a busid and exports. Anything reaching
that port could import a bound device, **bypassing benchd entirely**, so a lease would
claim one thing while the hardware answered to someone else. Handing the kernel our own
dialled-out socket (D5) removes the listening socket rather than firewalling it.

---

## 10. Open questions

None blocking.

| # | Question | State |
|---|---|---|
| Q10 | Relay latency for JTAG/high-rate transports | **Out of scope.** Serial only for now (~11 KB/s), where the extra RTT is noise. Revisit if a JTAG bench ever exists. |
| Q12 | Is our USB/IP handshake byte-correct? | **Yes.** Verified against stock `usbipd`/`usbip` and by a live loopback import: DTR/RTS auto-reset works through the relay and the ESP32 ROM banner comes back. |
| Q11 | Can `usbipd` bind loopback-only? | **Moot — we don't run it.** It binds wildcard with no auth, so instead both ends dial out and hand the kernel the resulting fd (D5). No listening socket to confine. |
| Q12 | Are the `OP_REQ_IMPORT`/`OP_REP_IMPORT` structs implemented byte-correctly? | open — differential-test against stock `usbip`/`usbipd` in both directions |

**Deferred ideas.** *Idle release* — TTL catches crashes, not an agent that claims a
board then reads source for 12 minutes; detect via no open fd for N minutes. *Sticky
reclaim* — a short soft-reservation after release so flash→test→tweak stays on one
board. *Bench affinity* — for two benches sharing an RF chamber. *Roles and
authentication* — see §9 for the order the seams should be taken in. All
revisit-with-evidence, not now.

---

## 11. Status

| Component | State |
|---|---|
| Tags, vocabulary, implications | done, tested |
| Bench/resource model, TOML config | done, tested (schema splits per D9) |
| Matcher | done — 21 tests + property test |
| Multi-resource benches | done, tested |
| Limits (one global set) | done, tested |
| Lease lifecycle + reaper logic | done — 15 tests |
| Workspace split | done (`crates/benchd-core`) |
| Wire messages + JSON line protocol | done, tested |
| Coordinator daemon | working end-to-end |
| Host / client daemons | specified |
| USB/IP handshake + sysfs fd handoff | done, verified on hardware |
| Skill | not started |

No persistence layer appears here, and that is the point of D6. No schema language
either, and that is the point of D5.

**Order:** workspace split → policy → wire messages → coordinator lease manager → host
→ client materialiser → MCP → skill.

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

## Appendix B — findings from the adversarial review

An adversarial review after the first working build found that most guarantees
this document asserts were **not enforced by the code**. The mechanisms existed —
epochs, `Outcome`, name sanitisation, grace windows — but were unwired, applied
to the wrong variable, or ordered wrongly. Recorded because the pattern matters
more than the individual bugs.

| Severity | Bug | Root cause |
|---|---|---|
| Critical | Agent-chosen slot names were path components in a root daemon: `mount --bind` at any location | Sanitised the *owner* name and tested it, then passed `slot` and `resource` through untouched |
| Critical | A normal agent exit left the mount live while the bench was marked free — two agents, one board | `session_conn` deleted before `dispatch`, which resolves the client through it |
| High | `Outcome::Failed`/`Stale` logged at debug and discarded: a failed claim held the bench for its whole TTL | Protocol carried failures nothing acted on |
| High | Epoch fencing died permanently after a coordinator restart | Host kept its high-water mark across reconnects; a stateless coordinator restarts at 1 |
| High | `epoch_for` stamped one bench's epoch on every bench in a claim | Epochs are per bench; a multi-slot relayed claim could never rendezvous |
| High | A one-second coordinator blip wedged every agent on a machine | Sessions invalidated with no path to re-register |
| High | Agents could claim by name, contradicting D17 | `name=` left in the open vocabulary |
| Medium | A slow materialisation blocked *all* coordinator messages for every agent | Messages handled serially in the read loop |
| Medium | `tick` advanced one state per call, so an overdue lease survived a late tick | Warning and teardown were mutually exclusive branches |
| Medium | Host leaked a usbip binding when export failed after binding | Teardown list excluded the busid being worked on |
| Medium | `force_release` could extend a lease past its own expiry | Grace added to `now` without capping at `expires_at` |

**The cause was uniform: every test exercised the happy path.** None asked what
happens when a step fails. `tests/failure_paths.rs` exists to keep that from
recurring, and immediately found the `tick` bug.

## Appendix C — corrections made during design

Both were caught by checking, not by thinking harder.

1. **"Multi-slot claims are reachable through labgrid's gRPC API without patching."**
   Wrong — the coordinator's own scheduling loop hardcodes `filters["main"]`, so it
   would have needed a coordinator patch.
2. **Unweighted scarcity scoring** would have made the matcher hoard the lab's cheapest
   board, because rarity stood in for value. Caught by running it against the real
   inventory.
