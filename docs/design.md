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
| **Resource** | One physical thing: a serial device, a USB device, a relay, a probe. Named by **position** (`by-path`) with the **serial** of the chip expected there — see D20. |
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
 │ benchd mcp   (per agent) │        │ COORDINATOR   (only listener) │
 │  · thin, unprivileged    │        │  · inventory + matcher        │
 └──────────┬───────────────┘        │  · limits + lease state       │
            │ local socket           │  · reaper                     │
 ┌──────────▼───────────────┐  JSON  │                               │
 │ benchd client (machine)  │───────▶│                               │
 │  · node materialiser     │  lines │                               │
 │  · runs as root          │  / TCP │                               │
 └──────────┬───────────────┘        └───────────────▲───────────────┘
            │ mknod                                  │ JSON lines / TCP
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
`benchd-host@mesh-rig.service`, so N benches is a config concern. Every box in the
diagram is the same executable under a different subcommand (D26).

**Same-machine is not a special case** — and now not even a separate code path. On one
box the client and host are just two processes, and the device still travels between
them over USB/IP, because a host hides its devices for its whole lifetime (D22) and so
has no local inode to offer. There is nothing to detect and nothing to choose (D4).

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

### D2. The imported node itself, at a per-lease path

Symlink `/run/benchd/<owner>/<coordinator>-<lease-id>/<slot>/<resource>` to the imported
device's node in `/dev`, exposed as `$LAB_DUT_CONSOLE`, and give that node to the leasing
agent's uid at `0600` for as long as the lease lasts.

*Why real nodes:* Goal 2 rules out everything else. `rfc2217://` and `socket://` are
pyserial-only — `idf.py monitor`, `minicom` and `openocd` refuse them. A pty bridge
loses modem-control lines, breaking ESP32 auto-reset intermittently.

*Why the node itself, and not a private `mknod` of it.* This was a private node until it
was measured against the toolchain, and a private node is by construction one that **no
enumeration lists** — which is most of its appeal and, it turns out, the reason it fails
Goal 2. A tool that identifies a device by looking around the system rather than by
opening the path it was handed then misbehaves in ways unrelated to permissions.
`esptool` resolves a console's USB vendor and product id by matching the path against
pyserial's port list, which globs `/dev/tty*`; against a private node it finds nothing
and assumes a USB-UART bridge. On a native-USB part that means both a reset sequence that
cannot reach the bootloader *and* — because the same VID/PID answer drives
`uses_usb_jtag_serial()` — skipping the RTC watchdog and SWD autofeed that such a part
needs disabled **while flashing**. Measured: an unmodified `esptool flash-id` times out
after 42s against a private node and succeeds in 3s against a symlink to the real one,
reporting `USB mode: USB-Serial/JTAG` only in the second case. `usbsdmux` fails the same
way for a different reason (D24). `--before usb-reset` papers over the first half only,
turning a clean failure into an intermittent one, which is worse.

*Why giving the node away is a smaller concession than it reads.* It is the same fact
that made locking it safe: this is never the machine's own hardware. Every node the
materialiser touches belongs to a device that exists here *only because this lease
imported it* over USB/IP, and that vanishes when the lease detaches its vhci port — its
lifetime is already exactly the lease's, and there is no other user of it to protect it
from. `0600` to the leasing uid therefore grants that uid precisely what the private node
granted it. The real objection to the old bind mount was never sharing the inode as such,
but *inheriting* the source's group and mode, which handed the board to all of `dialout`
or `disk`; both are overwritten here rather than kept. Verified on hardware: `tom:root
0600` for the lease's duration, back to `root:plugdev 0660` on release, and the node gone
a moment later when the port detaches.

*What it costs.* A stale reference gains a second way to be wrong. The lease path still
fails `ENOENT` once the lease ends, but a tool that resolved it to `/dev/ttyACM0` and
cached *that* could reach a later lease's board, since kernel names are reused. A
different agent is stopped by the ownership; the same uid is not. Accepted deliberately:
agents are handed `$LAB_DUT_*`, and no tool in this toolchain persists a realpath across
leases. Enforcement is otherwise unchanged — outside a lease the device is not imported
at all, so "forgot to claim" is still `ENOENT` rather than `EACCES`.

*The refusal that follows has no voice of its own, so the handover is logged instead.*
When somebody opens a reserved node anyway they get `EACCES` from the kernel, with
nothing of ours on the stack to explain it — there is no hook, and `kernel.dmesg_restrict`
keeps an unprivileged agent out of `dmesg` in any case. The daemon therefore names the
device, the lease and the uid when it takes the node, so that `journalctl -u
benchd-clientd | grep ttyACM0` answers the question. The audience is the operator.

*What it buys, beyond the tools working:* two sharp edges disappear with the private
node. The lease tree no longer has to be on a filesystem mounted without `nodev` — `/run`
is `nosuid,nodev` on any systemd machine, and a node created there could be created and
then never opened, which used to make every lease report success and every open fail
`EACCES` on a node that looked perfectly correct in `ls -l`. And materialisation needs
only `CAP_CHOWN`, no longer `CAP_MKNOD`.

*Why the lease id is in the path:* if `/dev/lab/dut` meant board A last lease and board
B this lease, a stale shell writes to the wrong board — the original failure,
reintroduced. Stale paths must fail `ENOENT`.

*Rejected:* handing out `/dev/ttyUSB0` directly, with no lease path at all. Kernel
indices renumber and collide, reintroducing the identity problem tags exist to remove;
the symlink is what gives the device a stable, lease-scoped name.

*Rejected:* leaving the private node and fixing `esptool` upstream to resolve VID/PID by
`stat`ting the path and reading `/sys/dev/char/<major>:<minor>`. That fix is real, works
(measured: it recovers `303a:1001` from a private node, through `vhci_hcd`), and would
subsume the special cases esptool already carries for udev aliases and Docker
`/host_dev` bind mounts — but it repairs one tool, on a release schedule we do not
control, while every other enumerating tool stays broken. Worth sending upstream on its
own merits; not a substitute for benchd handing over something that behaves like a
locally connected device.

### D3. Greenfield, not labgrid

*Why:* the piece we need most — a device node materialising in a client-side sandbox —
fits labgrid's architecture worst. Its model is "resource stays on the exporter, client
talks over the network through a Driver"; a root-side materialiser of device nodes is none of
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

*Not a trait any more:* the materialiser used to bind-mount a local inode when the host
and client shared a machine, and forward over USB/IP when they did not. Hiding (D22)
removes the choice — a hidden device has no tty, so there is no local inode to hand over
even on one box — and USB/IP is now the only delivery mechanism. One code path, no
topology detection, and the same bytes on the wire wherever the client happens to be.

### D5. Only the coordinator listens; newline-delimited JSON over plain TCP

The coordinator is the sole listener. Hosts and clients dial in and hold the connection
open; instructions travel back down it. Messages are `serde` enums in a shared crate,
one JSON object per line, over **plain TCP everywhere** — no unix-socket special case
for co-located components, no TLS. The listener binds `127.0.0.1`, so "plain" is a
statement about benchd's own code rather than about what crosses a network: peers on
other machines arrive through an SSH forward (§9).

*Why coordinator-only:* hosts live wherever the hardware is — lab VLAN, bench laptop,
behind NAT. Requiring each to be addressable makes adding a bench an infrastructure
task instead of a `systemctl start`.

*Why a held-open connection:* the coordinator must reach executors it cannot dial, so
pushing down an inbound connection is the only delivery mechanism available — not a
convenience. The same connection carries heartbeats up and `revoking` down.

*Why no TLS:* **benchd does no cryptography.** Rolling a PKI is a notorious time sink,
and the half that would matter — issuing *client* certs so hosts can be identified — is
the half nothing automates. The boundary goes underneath instead: the coordinator binds
loopback, and a remote host or client reaches it over an SSH forward (§9). The transport
is then encrypted and both ends mutually authenticated without a line of it being ours.
What that does not buy is peer identity — every connection arrives from `127.0.0.1` —
which §9 states outright rather than leaving to be discovered.

*And the tunnel runs the same direction as the dial,* which is why SSH fits here rather
than merely being available. The forward is opened *by* the bench machine, outward, so a
host behind NAT needs no more addressability with a tunnel than without one, and the
property this decision exists to protect survives it.

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
8.  client           mknod a private node for the imported device in the lease
                     directory, and lock the imported one in /dev to root
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

*Used locally too.* A co-located bench takes exactly this path, relay and all. Its
devices are hidden like any other (D22), so there is no local node to hand over
instead. The cost is the coordinator sitting in the data path for a board that is
physically in the same machine; the benefit is one code path rather than two, and no
inference about which machine anything is on. A direct host-to-client dial for the
loopback case stays available as a later optimisation, and would be a transport variant
of the same handle rather than a second materialisation path.

*And it is what makes locking the `/dev` node safe (D2).* Because every leased device
arrives over USB/IP even on one box, every node the client materialises from belongs to
an imported device rather than to this machine's own hardware — so taking it away from
non-root users for the duration removes nothing anybody else could legitimately have
been using.

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

*Losing the coordinator releases immediately, with no grace window.* An earlier draft
specified one: executors would hold for `G` seconds and, on reconnect inside `G`,
declare what they still held for the coordinator to confirm or deny. That was designed
and then not built, and on reflection it should not be — a documented guarantee nothing
implements is worse than an honest limitation.

It buys nothing here. The coordinator is stateless (below), so a restart denies every
lease anyway; the protocol would only help when the coordinator *process* survives but
the connection drops, which needs two machines. And both ends already agree without it:
an executor releases on disconnect, and the coordinator drops the sessions or benches
behind a disconnected peer. They converge on the same answer, so there is no window in
which one believes a lease is live and the other does not.

Revisit when hosts routinely run on other machines and a flaky link starts destroying
work that a few seconds of patience would have saved. Until then the reconnect-and-
reconfirm handshake is unwritten code carrying an unenforced promise.

*Liveness is measured by silence, not by sockets.* A TCP connection can outlive the
process behind it — a wedged host, a sleeping machine, a half-open connection. A host
that has not been heard from for `host_timeout_seconds` has its bench withdrawn from
matching and its leases ended, whether or not its socket is still open.

*Which means connections are tracked per connection, not per bench, and a bench survives
a reconnect.* A restarted host may take its bench back while the coordinator still holds
its half-open predecessor, so briefly two connections claim one bench. Keyed by bench,
the loser was simply forgotten — and then the *winner's* disconnect took the bench with
it, leaving the host that was still connected, still heartbeating and still physically
holding the hardware with no way to get it back. The displaced connection is kept as a
standby instead, which also means two hosts genuinely configured for the same bench take
turns rather than destroying it between them.

*A returning host does not get its bench back as immediately matchable if a teardown is
still outstanding against it.* The previous holder's device nodes do not disappear
because the host reconnected, so the hold outlives the connection that created it, and
the bench becomes matchable when the teardown is answered or when the hold expires.
Nothing waits forever: a client that will never answer — because it died, or because its
unmaterialisation failed outright — costs its bench that bounded wait and no more.

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

`benchd client` is one root daemon per agent machine, holding the coordinator
connection and doing all materialisation. `benchd mcp` is a tiny unprivileged MCP
server, spawned per agent by its harness, talking to the daemon over a local socket.
They are two subcommands of one binary (D26) and two processes at two privilege
levels, which is the part that matters.

*Why the daemon is privileged:* the device node must appear on the agent's machine
(Goal 2). Creating one needs `CAP_MKNOD` and handing it over needs `CAP_CHOWN`; `usbip
attach` needs root outright, and so does mounting the tree D2 requires. The agent stays
unprivileged, so something local holds privilege for it. It runs as root today: the
capability set above is what materialisation actually uses, not a confinement anyone has
imposed on it.

*Why the split is not optional:* **MCP over stdio is one process per client** — stdio
is a pipe pair, so the harness spawns the server. A single daemon cannot serve stdio
MCP to several agents. (An earlier draft of this decision said "one daemon, no shim";
that was simply wrong about how stdio works.) The shim stays thin and unprivileged;
privilege lives in the daemon.

*Rejected:* serving MCP over local HTTP so one daemon handles every agent. It works
(`rmcp` supports it) but pushes per-agent identity into a header the harness must set,
which is more fragile than a process boundary that already exists.

### D20. A bench names a position, and records what should be in it

A serial resource is named by its `/dev/serial/by-path` entry, and optionally
carries the USB `serial` of the chip expected in that position.

*Why position:* a bench **is** a physical slot — a port on a hub, with a board
cabled into it. `by-path` is stable across swapping that board, so replacing a
dead one needs no config edit. `by-id` names a specific chip instead, and breaks
loudly on a swap, which is right only when a particular board matters more than
the slot. Never `ttyUSB0`: kernel indices renumber on replug and hand an agent
the wrong board.

*Why also the serial:* position stability is exactly what makes a swap silent,
and the tags describe the **chip**, not the slot. Observed in practice on this
lab: the same port held three different chips over one session while its config
kept asserting `soc=esp32s3`, so agents asking for an S3 were handed a C3 and a
P4 — the precise failure tags exist to prevent. With a serial declared, a swap is
refused at registration naming both serials and pointing at the tags; without
one, whatever is in the slot is accepted.

*Not chip-ID probing:* reading the ROM banner would give ground truth, but it
requires resetting the board, which is not something to do to hardware at
registration, and native-USB parts re-enumerate when reset. A declared serial
costs one config line and makes the drift loud, which is enough.

### D19. Identity is a session token; the name is only a label

At startup `benchd mcp` registers with the coordinator, declaring a name from
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

### D21. A client speaks to several coordinators; the agent sees one lab

A `benchd client` dials any number of coordinators at once. The expected setup is two:
a shared lab server, and a coordinator on the operator's own machine, bound to
`127.0.0.1`, owning the boards on that desk.

*Why:* the requirement is "my own boards stay mine, the lab's are shared". Doing that on
one coordinator means bench ownership, which means real identity, authentication and
per-bench ACLs — the whole apparatus D19 exists to avoid, to obtain a property that
binding to loopback gives for free. A local coordinator is not reachable from the
network at all, so the lab server never learns those boards exist and no policy has to
be written down.

It also buys **failure isolation**, which matters more than it first appears. D6 makes
the coordinator a SPOF whose death releases every lease it owns. With one coordinator,
lab downtime — or simply being on a train — takes the boards on your own desk with it.
With two, a lab outage is invisible to local work, and the daemon reconnects to each
independently. This is verified: killing the lab coordinator tears down its lease and
leaves the local one materialised and usable.

**One bench, one authority.** A host registers with exactly one coordinator. Registering
with two would let two authorities grant a lease for the same hardware simultaneously,
which is the single-writer property of D6 — the whole point of the system. The unit of
division is therefore the *access policy*, not the team: benches that can be claimed
*together* must share a coordinator, because an atomic multi-slot claim (D13) cannot
span two authorities without two-phase commit, which is not worth it here.

**Agents are not told.** `tag_list` returns the union with counts summed; `lease_status`
merges; a claim is offered to coordinators in configuration order and the first that can
satisfy it wins. Listing the local one first therefore prefers your own hardware. A claim
is never broadcast: two coordinators satisfying it at once would hold hardware nobody
asked for. Failures merge by D14's rule — retryable if *any* coordinator is merely
contended, unsatisfiable only when they all agree.

**Exactly one answer per request, and never a merged view that hides a gap.** The
single-lab illusion is built in the client daemon, which is also where it can break, so
the property is stated as an invariant: no request may be answered twice, none may go
unanswered, and a request that only some coordinators answered is not an answer. The
failure that motivates the third clause is the quiet one — a `tag_list` that lost a
coordinator returning the benches it *could* reach, so an agent sees a lab that has
silently shrunk and concludes the board it wants no longer exists. It now reports the
failure instead.

*Everything is completed by an event, so there is one deadline for the case where no
event comes.* A fanout completes on its last reply, a claim on a grant or on running out
of coordinators to offer it to; a coordinator that stays connected and simply never
answers is not an event. After 20 seconds the daemon answers the request itself. That is
comfortably inside the MCP shim's own 30-second timeout, so the agent is given a reason
rather than a timeout, and it is the only deadline on the daemon's side — `benchd lease`
has none at all and waits for its request id indefinitely. A coordinator that is
connected but has not finished re-registering is waited for separately and briefly, since
one that accepts a connection and never completes the handshake must not hold up every
request on the machine.

*A lost link cannot leave a request outstanding.* When a coordinator link drops, anything
in flight to it — an outstanding claim, a renewal, a release — is resolved rather than
abandoned, and the link is retired from the fanout tables before the daemon spends
seconds tearing its leases down, not after. A claim that has not been satisfied moves on
to the coordinators that remain instead of dying with the one that went away.

**Ids are per-coordinator and must be namespaced.** Each coordinator is an independent
authority numbering its own `LeaseId`, `SessionId` and epochs from 1, so `l1` is
ambiguous the moment a second coordinator exists — and silently so, which is how the
epoch bug in Appendix B behaved. The client therefore keys every map on a composite
`(coordinator, lease)`, and materialises to `…/<owner>/c1-l7/…`. The id handed to the
agent packs the coordinator into the high 32 bits, so the agent still passes one opaque
number to `release` and never learns there is more than one lab. This was not
theoretical: the first end-to-end run had both coordinators issue `l1` simultaneously.

### D22. A bench's devices are hidden by the host, not concealed by a sandbox

`benchd host` binds every device its bench declares to `usbip-host` at startup and does
not give them back until it exits. A stub-bound device has no driver claiming its serial
interface, so it has **no tty at all** — not a `root`-owned one, not a mode-`0600` one.
There is nothing on that machine to open, for another agent, an unprivileged user, or
root.

*Why absence rather than permissions:* every permission scheme leaves the node present,
which means `root` and `CAP_DAC_OVERRIDE` still open it, and — worse — a stale
`/dev/ttyUSB0` in a script still resolves to a real board. Absence has no such holes, and
needs no policy to stay correct.

*Why this replaces the client-side sandbox:* the sandbox existed to hide bench devices
from an agent's `/dev`. With nothing to hide it has no job left, and benchd no longer
requires one to function. Separating two agents that share a uid on one client machine is
a different problem, and one the operator should solve with whatever sandbox they prefer
rather than one this project mandates. `dist/benchd-sandbox` remains as a worked example
— and is at present a *broken* one, because D2 now hands out a symlink into `/dev` and
that script's whole method is to give the agent a `/dev` without the device in it. The
repair is a per-agent `/dev` holding that agent's own leased nodes under their kernel
names, which would serve enumeration and same-uid isolation at once; it is not written.
Agents with distinct uids are unaffected, being separated by the node's ownership.

*Cost, accepted:* an unleased board is unusable on its own host without going through
benchd, and `benchd host` now needs root even for a bench that never leaves the machine.

**One owner for the stub binding, and it is hiding — not exporting.** A lease attaches a
socket to a device that is *already* bound, and ending one takes that socket away again;
the binding itself is made once when the host starts and undone once when it exits. So a
device changes driver exactly twice in a host's life, and both times in the same file.
This is stated as an invariant because violating it did not look like a bug: when
exporting owned the binding too, ending a lease unbound the device outright, `cdc_acm`
claimed it, a `/dev/ttyUSB0` reappeared on the host — and nothing in the system would
ever hide it again. Every lease after the first silently gave back exactly the guarantee
this decision exists to provide. The fifth-pass review found it; no test had ever ended a
lease and then looked at the host.

*Why release must be belt and braces:* a stub binding is kernel state that outlives the
process which made it, so a host that dies without unbinding leaves boards invisible and
nothing running that remembers why — the D6 problem, applied to hiding. Three layers
answer it. The busids are written to `/run/benchd-host/<bench>.busids` **before** they are
bound, so the record can never be missing for a device that is; a SIGTERM handler releases
on the ordinary shutdown path; and `ExecStopPost=` runs `--release-all` even when the
process was killed outright. A power cut needs none of them, because stub bindings do not
survive a reboot and neither does the `/run` record — which is exactly why it lives there.

*Why a refused registration unhides:* a bench the coordinator will not accept cannot be
leased by anyone, so keeping its boards hidden serves nobody and removes the hardware
from the machine as well. Worse, it hides the cause: a typo in a tag would make the
boards silently vanish. So registration being refused gives the devices back and retries
slowly, which both keeps them usable by hand and lets a corrected vocabulary recover on
its own. Losing the *coordinator* does not unhide, because that says nothing about
whether the bench is valid and no lease can exist while it is gone.

*Why a host refuses to manage a forwarded device:* now that co-located leases go over
USB/IP like everything else, a bench can be imported back onto the very machine it is
plugged into — the ordinary case for the private local coordinator of D21. The imported
copy is faithful: same vendor, same product, same serial, and therefore the same
`/dev/serial/by-id` name as the board it came from. On any other machine that is harmless
and even useful; on this one it means a `by-id` resource can resolve to this bench's own
copy during the seconds a lease is being torn down. Hiding *that* would stub a phantom
and leave the real board on its driver, visible to everything hiding exists to keep it
from — so resolution rejects any device sitting under `vhci_hcd`, and `wait_for_hardware`
simply retries until the board itself is back. `by-path` benches were never exposed,
since a virtual device has a different physical path.

*Why the record, and not a search:* recovery used to match stub-bound devices by looking
for their USB serial inside the declared path, which only works for a `by-id` name because
only those embed one. `by-path` is the recommended way to declare a bench, and it never
matched — a latent gap that became a restart-path failure the moment hiding made recovery
routine. An explicit record needs no heuristic and works for both.

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

*What a host may declare is checked on registration, and one tag it may never declare is
its own name.* `name` is an open key — bench ids cannot be enumerated centrally, so it is
the one tag a declaration cannot be validated against — which means a declared
`name=esp32s3-a` simply **is** `esp32s3-a` everywhere matching happens. Since anyone who
can reach the coordinator may register a bench (§9), a host that declares someone else's
name captures every claim aimed at that board. The real `name` is injected from the bench
id by whoever knows what that id is, and a declared one is refused. Both places that
build a bench from declared tags have to apply this — the inventory loader and the
coordinator's host registration — and until the fifth-pass review the registration path
did not.

### D10. Tag vocabulary is closed, `key=value`, with implications

Unknown tags are rejected with did-you-mean suggestions.

*Why:* free-form tags rot within a week once *agents* write the requests. A closed
vocabulary turns a typo into a self-correcting error in one turn instead of ten turns of
"no bench matches".

*Deliberate asymmetry:* bench tags are expanded, requirements never are — expanding a
request makes it strictly harder to satisfy.

*Dashes:* legal in values generally (`name=esp32s3-a` is an opaque identity string),
rejected in *vocabulary* values and in qualifiers, which is where the
`esp32-s3`/`esp32s3` split happens.

#### What a board *is*, versus how it is *wired*

The vocabulary is in three parts, and the split is load-bearing:

| | example keys | declared by |
|---|---|---|
| Silicon | `soc`, `family`, `arch`, `net` | implied from `soc=` |
| Board build | `psram`, `flash`, `peripheral` | the bench |
| Wiring | `console`, `jtag` | the bench |

Only silicon implies anything. `soc=esp32s3` ⇒ `family=esp32`, `arch=xtensa`, `net=wifi`,
`net=bt`: these follow from the chip identity and nothing at the bench can make them
false.

`soc=esp32s3` used to also imply `jtag=builtin` and a `usb=native` key that no longer
exists, and that was wrong. The
S3 *has* a USB-JTAG peripheral, but whether this bench can reach it depends on which of
the board's two sockets the cable is in — a fact the SoC cannot possibly know. The
implication made every S3 advertise debug access, so a claim for `jtag=builtin` could be
answered with a board wired through its UART bridge, and the agent found out by failing to
attach. An implication that is true of the silicon but false of the bench is worse than no
implication at all, because it is asserted with the coordinator's authority.

The rule that falls out: **implications encode "is a kind of", never "is connected to"**.

*Absence is not a value.* A bench with no debug access omits `jtag` rather than declaring
`jtag=none`. Best-fit scoring (D12) prices every tag a bench carries but the request did
not ask for, so declaring an absent capability would make the *less* capable board score
as the more precious one.

#### Qualified values: `peripheral=accel[mpu6050]`

Categories are curated centrally; parts are not. A bench writes
`peripheral=accel[mpu6050]`, where `accel` must exist in the vocabulary and `mpu6050` is
free-form. Keys opt in with `qualified = true`.

*Why:* the alternative is a central list of every accelerometer, flash chip and GNSS
module anyone might solder to a board, maintained by someone who cannot see the board.
Curating `soc=` pays for itself — three values cover the whole lab and each stands in for
four more tags — but curating parts is unbounded work whose only reward is that someone
else's bench config becomes legal. The person looking at the hardware already knows the
part number; the vocabulary just needs to stop being in their way.

At load time a qualified tag desugars into two ordinary tags on the bench,
`peripheral=accel` and `peripheral=accel[mpu6050]`, so matching stays plain subset
containment and the matcher needs no notion of qualifiers at all. Combined with the
asymmetry above this gives exactly the behaviour you want in both directions: a request
for the category matches the board that recorded its part, and a request for the exact
part does *not* match a board that only claims "an accelerometer".

Scoring treats the pair as one chip. Charging for both would mean a board scored as
scarcer for having its part number written down, which teaches everyone to stop writing
part numbers down.

### D23. A bench documents itself, and the documentation arrives with the grant

A bench config may carry a `docs` string of markdown — pinout, jumper positions, what is
soldered to what. It rides the registration to the coordinator and is handed to the agent
in the `granted` reply, per slot.

*Why the bench config:* the same reason the tags live there (D9). The notes and the wiring
they describe are the same edit; a central document is out of date the first time someone
moves a jumper.

*Why grant time, and not discovery:* tags say what a bench *can do*, which is what an
agent needs in order to ask for one. Wiring notes say what is *on* a particular bench,
which is only useful once you have it — and publishing them in `tag_list` would hand every
agent a per-bench fingerprint to select on, quietly undoing D17's rule that agents describe
hardware rather than name it. Delivering at grant time also means the notes are scoped to
the slot: the agent is told what is wired to `dut`, not what exists in the lab.

*Why capped at 8 KB:* this text lands in an agent's context window, uninvited, on every
claim. That is a budget someone else is spending, so there is a ceiling — enforced on the
host *and* re-checked at the coordinator, since the host may be an older build. Anything
longer belongs in a repository the docs can link to.

*Why the operator CLI flags the absence:* a bench with no notes is invisible until an agent
wastes a lease guessing at its pinout, so `benchd inspect` marks it `[no docs]` rather than
saying nothing.

### D24. A resource names one device node; several may name one device

A resource resolves to exactly one path an agent can open. A USB device that produces
several nodes is therefore described by several resources sharing one `busid`, and the
node each one wants is named explicitly: `kind = "scsi"` or `kind = "block"`.

```toml
[resources.sdmux]           # /dev/sg0 - usbsdmux switches the card through this
kind  = "scsi"
busid = "3-1.1"

[resources.sdcard]          # /dev/sda - the card itself, once switched to the host
kind  = "block"
busid = "3-1.1"
```

*Why not one resource for the whole device:* it would have to resolve to a directory,
and the agent would then have to pick a node out of it — a choice it cannot make
correctly, because it cannot see the device. `usbsdmux` needs the SCSI node and `dd`
needs the block node; a resource that names neither has moved the problem rather than
solved it.

*What follows from it:* USB/IP forwards whole devices, so resources sharing a `busid`
share a single channel. The coordinator mints channel keys per device rather than per
resource and repeats the key across them; the host binds the device once; the client
imports once and resolves each node from that one vhci port. Both ends deduplicate on
the key, so neither needs a second concept for "these two go together".

*Why the host records a tty's USB interface:* the same device can offer two serial
ports. An FT2232H exposes its two channels as interfaces 0 and 1, and on a WROVER-KIT
one is JTAG and the other is the console — so locating the tty after an import by
taking whichever the kernel lists first is a coin toss, and losing it hands the agent a
port that will never speak to it. The `by-path` in the config already names the
interface, so the host reads it while the hardware is still visible and the client asks
for that interface by number. Nothing falls back to the other interface when the wanted
one has no tty: the fallback is the ambiguity.

*Why nodes are located by vhci port, never by name:* a forwarded device reproduces the
`by-id` name of the board it came from, so diffing `/dev/serial/by-id` across an import
can see nothing at all. Storage is worse — the machine running the client very likely
has a `/dev/sda` of its own, and taking it would put the wrong disk in a lease directory
— and, since D2 locks whatever it materialises from, take the machine's own root disk
away from everything else on it.

*Why `kind = "usb"` is now an error:* it meant "the whole device" and had no way to say
which node was wanted, so such a bench registered without complaint and then spent ten
seconds at materialisation waiting for a tty that would never appear. Failing at config
load is the same error, ten minutes earlier, with the two spellings that would fix it.

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

### D12. Best fit, priced by wasted capability

Among adequate benches pick the least capable: cost is `Σ weight(key) / (benches carrying
that exact key=value)` over the tags a claim does not ask for. Matching itself is **exact
per tag** — there is no ordering between values, and nothing in the matcher knows that
`8mb` is more than `4mb`.

*Why:* first-fit hands the only JTAG bench to an agent that asked for a blinking LED.

*Why weighted — a bug we hit:* pure scarcity conflates *rare* with *valuable*. Being the
only CP2102N board made `console=uart` unique, so the matcher protected the lab's cheapest
board and handed out the PSRAM one. Identity keys (`soc`, `arch`, `console`) get weight 0;
contended peripherals keep 1.

*The weight is the only knob, and the vocabulary has to hold up both ends of it.* Because
matching is exact and the denominator counts benches carrying that exact `key=value`, a
key whose values are *ordinal* prices the rarity of a value rather than any capability:
`flash=4mb` on one board costs twice `flash=8mb` on two, which is "least capable wins"
running backwards. So `flash` is weight 0 along with the identity keys, and any key like
it belongs there too. Only keys naming a capability a claim can waste — `peripheral`,
`jtag`, `psram`, `sdmux` — keep weight 1.

*And a value naming the absence of something must not exist.* `psram=none` or `jtag=none`
is charged exactly like a real capability, so a board that has nothing is priced as
though it had something rare. A board without the hardware omits the key entirely. This
is a vocabulary rule the matcher cannot enforce, so the shipped vocabularies say it where
the temptation is; `psram=none` was in them until the fifth-pass review.

*Why the denominator counts only benches that could actually serve.* It is over benches
enabled and not busy, not over the whole inventory. Pricing against benches nobody can
have makes a capability look plentiful precisely when it is scarcest.

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

*The distinction is the surface, not the caller:* both live in the operator commands,
which an agent is never handed. Since D26 those ship in the same executable as the MCP
shim, and this is undisturbed by that: what an agent can reach is the tool list its
harness gives it, and it was never the set of programs on the machine.

### D25. A person holds a bench through the agent surface, not the operator one

`benchd lease` claims by capability, cannot name a bench, and has the same operations an
agent has. It is a *presentation* of the agent surface — a terminal instead of MCP — and
so is bound by D17 rather than an exception to it.

*What it is not is an operator command that happens to be spelled differently.* They
talk to different things: `benchd benches` speaks to a coordinator over TCP and answers
"what exists, who has it, take it back", whereas claiming goes through the local client
daemon, because the point of a claim is device nodes appearing on *this* machine. D26
put them behind one command anyway, and the distinction survives it intact, because it
was always about what each one can ask for and never about how it is invoked.

*Why the process is the lease:* the socket connection is the session, so quitting,
crashing, or closing the terminal returns the hardware immediately. It is the same
mechanism that reclaims a bench when an MCP shim exits, and it makes the common human
failure — walking away — cost nothing.

*The TTL stays mandatory (D15) but the CLI supplies a default*, because a person at a
terminal is not the runaway that limit is for; here it is the backstop for a death that
takes the socket with it silently. Auto-renew is still refused: a holder who wants
longer says so.

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
                      operator.rs: benches / leases / release
benchd-host         owns one bench; export/teardown; power/mux
benchd-client       privileged: coordinator link + materialiser
                      lease.rs: hold a bench by hand (unprivileged)
benchd-mcp          thin per-agent stdio MCP shim (unprivileged)
benchd              the dispatcher; the only crate that produces a binary
```

With no `.proto` (D5) the wire messages are just `serde` types and live in core, which
already depends on serde for config. Core stays free of tokio so its property test stays
millisecond-scale. The two hand-operated surfaces live in the crate whose daemon they
talk to — the operator commands with the coordinator, `lease` with the client — because
each is a thin front end over that daemon's protocol and shares its types.

### D26. One binary, a subcommand per component

Every component ships as `benchd <subcommand>`: `coordinator`, `host`, `client`, `mcp`,
`lease`, and the operator commands `benches` / `leases` / `release`. Each is still its
own crate, and `crates/benchd` is a dispatcher that owns exactly two things they must
not each decide: the tokio runtime, and that logs go to stderr.

*Why:* a lab is several machines running different subsets of the components, and six
binaries meant six versions to keep in step across them. Every stale-binary dead end
this project has had — and there have been several, which is why `deploy.sh` verifies
checksums and restarts units — was one artefact being older than the others. One
artefact cannot be half-upgraded, and the wire protocol between a host and a coordinator
of different vintages is no longer something that can happen by accident on one machine.

*What it costs:* a host installs the MCP shim it will never run, and a few hundred
kilobytes of clap tables. Against having to reason about which of six things on a given
box is current, that is not a real price.

*It does not, on its own, cure the disease.* One binary removes the possibility of six
*versions*, not the possibility of the *wrong* version, and both places that produce or
consume the artefact had to be fixed separately afterwards. The install scripts assumed
`./target` and so installed and checksum-verified a file the build had never written
whenever `CARGO_TARGET_DIR` was set — agreeing with themselves perfectly. The integration
tests spawned `target/<profile>/benchd` by path from a crate that does not declare it, so
cargo never rebuilt it and the tests silently graded whatever was lying there. Both now
ask cargo where the binary is: the scripts through `cargo metadata`, the tests by living
in `crates/benchd` and using `CARGO_BIN_EXE_benchd`.

*What it does not cost:* the agent boundary (D17). It is tempting to read "the operator
commands are in the same executable an agent runs" as a weakening, but the executable
was never the control. An agent could always have run the operator CLI; what stops it
hardcoding a bench is that its harness hands it five MCP tools and no shell. Merging the
files leaves that exactly where it was.

*Rejected:* busybox-style dispatch on `argv[0]`, with symlinks named after the old
binaries. It would have made the migration a no-op, but it makes `benchd --help`
untruthful and turns a wrong-symlink deployment into a mystery. A clean cut, with
`deploy.sh` deleting the superseded names, fails loudly instead.

*Rejected:* keeping the daemons separate and merging only the two CLIs. That is the
split that causes the trouble — the daemons are the ones spread across machines.

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
and teardown only. Unexports everything at startup, and **hides its bench's devices for
its whole lifetime** (D22) so they cannot be reached on that machine except through a
lease.

> **Device loss is watched for, reported, and waited out.** The host polls its
> resources; when one disappears it sends `DeviceLost`, and the coordinator
> withdraws the bench and releases its leases so the matcher routes around it.
> The host then *waits* for the hardware rather than exiting: `Restart=always`
> would turn an unplugged board into an endless restart loop, whereas waiting
> means a board that is unplugged and plugged back in recovers on its own and
> the bench is simply not offered in between. Verified by unbinding a board from
> its USB driver: withdrawn in ~5s, back automatically on rebind, zero restarts.
>
> The watcher polls the **USB device in sysfs, not the tty**, and that is now
> structural rather than a refinement: a hidden bench (D22) has no tty at any
> point in its life, so a watcher looking for one would report every board lost
> a few seconds after startup. The USB device node stays put whichever driver
> holds it and vanishes only when the board actually does.
>
> Losing the hardware is also the *only* thing that unhides a bench. A coordinator
> restart must not, because no lease can exist while it is gone and briefly
> exposing every board to the machine would undo the point. An unplug must,
> because the `match_busid` entry outlives the device and would otherwise let the
> stub grab the board the instant it is replugged — leaving it with no tty and
> nothing able to resolve it.

**Client** — `benchd client`, one privileged daemon per agent machine, holding the
coordinator connection. Materialises and revokes; renews only on explicit agent call;
its unit mounts a `dev`-permitting tmpfs over the lease tree, since `/run` is `nodev`
everywhere and D2 needs real nodes. `benchd mcp` is
the thin unprivileged stdio shim spawned per agent (D8), which registers a session and
forwards calls over a local socket.

> **Startup cleanup follows a written record, not a sweep.** Removing "every
> materialisation under the root" is right for the directory tree but wrong for
> the vhci ports behind it: the port numbers are machine-global, so detaching
> every busy one takes down whatever a second daemon, or a person with `usbip`,
> is doing. The daemon records what it attached and detaches only that.
>
> **Work on one lease is serialised.** Materialisation and teardown for a lease
> are ordered against each other, so a teardown cannot overtake the
> materialisation it is meant to undo and leave a node behind. Different leases
> still proceed in parallel; the ordering is per lease, not global, because a
> USB/IP handshake that stalls must not wedge every other lease on the machine.

> **No sandbox is required.** Bench devices are hidden at the host (D22), so there
> is nothing on the agent's machine for a sandbox to conceal: an agent that ignores
> the skill and reaches for `/dev/ttyUSB0` finds no such device anywhere, because
> the board it names has no tty until a lease imports one.
>
> `dist/benchd-sandbox` remains as a worked example for operators who want to
> separate agents that share a uid, which is the one thing hiding does not do —
> and which matters more since D2, because a lease's device node now lives *in*
> the lease directory rather than being a mount of something in `/dev`. It uses
> bubblewrap: `--dev /dev` gives a fresh minimal `/dev`, a `--tmpfs` over the
> lease root hides every other agent's directory, and only the caller's own is
> bound back in, so leases appear and disappear inside a running sandbox with no
> restart and no cooperation from the agent. Nothing in benchd assumes it ran.
>
> It must be `--dev-bind` for that directory rather than `--bind`, because
> bubblewrap adds `MS_NODEV` to a plain bind and a created node is then
> unopenable — the same `nodev` trap as D2, one layer out. A bind mount would
> have survived either way, which is part of why the change was worth making
> deliberately rather than discovering later.
>
> The owner directory is named from the agent's declared identity alone, never a
> session id: a sandbox has to bind it at launch, which is before the coordinator
> has issued a session. It is created **by the daemon**, on request, root-owned
> and not agent-writable — an agent that can write its own lease directory can
> plant a symlink where root will create the next one, which is exactly how the
> fourth pass's second escalation worked. The nodes inside are `0600` owned by
> the agent's uid, and the source node's own group and mode are deliberately not
> copied: that group is `dialout` or `disk`, and copying it would hand the board
> to everyone in it precisely as the bind mount used to.

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
| Opening `/dev/ttyACM*` for a board you hold | `EACCES` | use the lease path; the `/dev` node is reserved on purpose (D2) |
| Operator forced a release | lease `revoking`, then gone | stop, park the board, re-claim later |
| A component restarted | lease gone, `lease_released` on next call | re-claim; work since last checkpoint is lost |
| Bench hardware failed | lease released, bench leaves `tag_list` | re-claim; the matcher routes around it |

The `ENOENT` row matters most: without it agents read revocation as broken hardware and
start power-cycling boards to fix a timeout. The last two mean a lost lease is routine,
not an error to escalate.

---

## 9. Security posture

**The boundary is SSH, and benchd still does no cryptography.** The coordinator binds
`127.0.0.1` and nothing else. Hosts, clients and operators on other machines reach it
through an SSH forward and arrive on loopback like everything co-located. What the
transport gives is exactly what SSH gives: the link is encrypted, and both ends
authenticated each other before a byte of benchd traffic crossed it.

**Inside that boundary there is still nothing, and that is deliberate.** Anything that
can open the forward can register a session, claim hardware, register a bench, and
force-release somebody else's lease. The rule is *access to the coordinator machine is
enough*: authorisation is delegated to that machine's accounts and `authorized_keys` —
a boundary a lab already knows how to operate — rather than to a second one invented
here and maintained by us.

What this buys: no PKI, no certificate distribution or rotation, no enrolment state to
persist and reconcile against D6's statelessness, no auth code in the hot path, and no
security theatre implying a finer boundary than there is. What it costs is listed
plainly, because a tunnel invites being mistaken for more than it is.

**What the tunnel does not do.**

- **It does not tell benchd who anyone is.** Every connection arrives from `127.0.0.1`,
  so the coordinator cannot tell a host from a client from an operator by origin, nor
  one host from another. A host may still register any bench id, including one meant
  for somebody else's board. Seam 3 below is unaddressed, not solved.
- **It is not scoped to a user.** A forward bound to loopback is reachable by *every*
  local user of the machine holding it. On a single-purpose bench machine that says
  nothing new; on a shared workstation it means "access to the coordinator machine" is
  really "access to any machine with a tunnel".
- **It does not survive being turned off.** `--listen` still takes any address, and a
  coordinator bound off loopback is exactly as exposed as it was before. The default is
  the safe posture; the flag is the loaded gun.

There are no roles (D15), so identity grants nothing — the session name is a label for
diagnostics, not an authorisation input (D19). Limits are global and apply to everyone
equally; they exist to stop a runaway agent hoarding boards, not to stop an attacker.

If this ever needs a finer boundary than a machine, the seams are deliberate and in
this order:

1. **A different network underneath**, if SSH proves the wrong shape — WireGuard peer
   public keys are already mutual authentication, and benchd would still do no
   cryptography. Not chosen now because it is a second network to run, and the identity
   it offers reaches benchd as a source address: a property of the tunnel's
   configuration rather than of benchd, so a misconfigured `AllowedIPs` becomes an
   authorisation bug. SSH needs no address plan and is already on every machine.
2. **`Register { name, credential }`** — the registration call already has the shape
   (D19); adding a check is one function.
3. **Host authentication** matters more than client authentication — a lying client
   harms only itself, a lying host can advertise benches that don't exist and mislead
   an agent about which board it is driving. Making `host_id` part of a bench's
   qualified id would make this structural rather than checked: a host that can only
   name benches inside its own namespace cannot forge one in anybody else's, and the
   `name=` tag D9 forbids a host to declare becomes derivable rather than unvalidatable.
4. **Mutual TLS in the application**, if the boundary must be finer than a machine at
   all. This is possible despite D5 handing the data socket to the kernel, which is not
   obvious and was checked rather than assumed: `stub_dev.c` and `vhci_sysfs.c` test
   only `socket->type != SOCK_STREAM` and never the address family, so a daemon can give
   the kernel one end of an `AF_UNIX` socketpair and pump the other through a TLS
   session — keeping the kernel's USB/IP implementation entirely, at one userspace copy
   per URB. Not done because it is materially more code than the problem justifies
   today, and because it costs the property that a handed-over socket outlives the
   daemon that created it.

Hosts and clients run as root but only execute epoch-qualified instructions from the
coordinator, never agent-supplied strings, and build paths only from validated
identifiers. Containment checks resolve the path before comparing it rather than
comparing the string, because the agent owns its own lease directory and a lexical check
follows a planted symlink. That is defence against *bugs*, which remains worthwhile
regardless, and it is where every escalation found so far has actually lived.

**Two of the trust assumptions above are narrower than "no malice" suggests, and are
enforced anyway.** A host may not declare its own `name` (D9), because that tag is
unvalidatable by construction and forging it redirects other people's claims — the one
place where the no-malicious-hosts assumption would have cost something a single check
prevents. And a lease's device node is private to the lease and the `/dev` node it came
from is locked to root for the duration (D2), so a second agent sharing a uid on the same
machine does not reach a board it was not granted. Neither makes benchd safe on a hostile
network; both remove a footgun that a *non*-malicious mistake would otherwise fire.

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
| Q10 | Relay latency for JTAG/high-rate transports | **Measured for flashing; the relay is not the limit.** 256 KB of incompressible data written to a board on the far side of the SSH forward: 637 kbit/s over a board's own USB, and over an FT2232H 88.9 kbit/s at 115200 but 526 kbit/s at 921600. The default baud is the ceiling there, not the RTT. Still genuinely open for JTAG, which no bench exercises. |
| Q12 | Is our USB/IP handshake byte-correct? | **Yes.** Verified against stock `usbipd`/`usbip` and by a live loopback import: DTR/RTS auto-reset works through the relay and the ESP32 ROM banner comes back. |
| Q11 | Can `usbipd` bind loopback-only? | **Moot — we don't run it.** It binds wildcard with no auth, so instead both ends dial out and hand the kernel the resulting fd (D5). No listening socket to confine. |

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
| Matcher | done — 42 tests including a property test |
| Multi-resource benches | done, tested |
| Limits (one global set) | done, tested |
| Lease lifecycle + reaper logic | done — 15 tests |
| Workspace split | done (`crates/benchd-core`) |
| Wire messages + JSON line protocol | done, tested |
| Coordinator daemon | done — hostile-input tests against a live daemon |
| Host / client daemons | done, deployed, verified across two machines |
| USB/IP handshake + sysfs fd handoff | done, verified on hardware |
| SSH-tunnelled transport | done — verified between two machines: an ESP32-S3 on a second host leased over the forward, control plane and USB/IP data channels both, with that coordinator bound to loopback and unreachable directly. `permitopen` refuses any other port (`administratively prohibited`) and the forced command yields no shell. A killed tunnel is restored by systemd in ~4s and leases resume with nothing else restarted |
| Flashing a remote board | done — verified through the forward with unmodified `esptool`, on both a native-USB ESP32-S3 and an FT2232H-bridged ESP32. 256 KB of random data, which esptool declines to compress and so sends whole, written to an erased region: `Hash of data verified`, read back byte-identical, region restored |
| One binary, a subcommand per component | done (D26) |
| Skill | done — `skill/benchd/SKILL.md` |

160 tests run on every `cargo test`, plus 19 that spawn real daemons and are `#[ignore]`d
so a plain run stays hermetic. Those 19 live in the `benchd` crate rather than beside the
code they exercise, which is not tidiness: cargo only guarantees a freshly built binary
to tests in the crate that declares it, and anywhere else they silently test whatever was
in the target directory. That was not hypothetical either — it is how the fifth pass's
fixes came to be reported as failures hours after they were merged.

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
| High | With two coordinators, `lease_status` reported every lease as belonging to the first | Lease ids rewritten to public form on `Granted` but not on the merged `Status` — same invariant, second place |
| Medium | `force_release` could extend a lease past its own expiry | Grace added to `now` without capping at `expires_at` |
| Medium | Relay keys were derived from `(lease, epoch, bench, resource)` — all small, sequential or discoverable — so a third party could guess a live key and be spliced in place of the real device | Convenience of independent derivation was preferred to unguessability |
| Medium | The client never fenced on epoch at all | Only the host implemented D7; the client relied on TCP ordering |
| Medium | No liveness tracking: a wedged host with a live socket kept its bench matchable | Heartbeats were accepted and discarded |

### Second pass: verifying the two things I had called unverified

| Severity | Bug | Root cause |
|---|---|---|
| High | Teardown effects carried epoch 0, so the client's own fencing rejected them and the mount outlived the lease | The epoch was looked up *after* the lease was removed — a regression introduced by adding client-side fencing |
| High | `free_vhci_port` ignored the hub column and could hand a full-speed device a SuperSpeed port | vhci lists both root hubs in one table; a single device always worked, a second one sometimes did not |
| High | A host killed mid-export left its board bound to the stub with no tty, and identified its device *through* that tty — so it refused to start forever and the board stayed dead | Recovery keyed on the thing the failure destroys |
| High | A sysfs write to a wedged usbip driver blocks forever, hanging a host at startup while systemd reports it active | No timeout on writes that normally take microseconds |
| Medium | A bench whose previous host connection was half-open could not be re-registered until the liveness timeout | Registration refused duplicates instead of replacing them |
| Medium | `unbind` left devices with no driver at all when `drivers_probe` raced the stub teardown | Single attempt, result ignored |

### Third pass: the first test across a real network

| Severity | Bug | Root cause |
|---|---|---|
| Critical | The coordinator discarded any payload the line codec had already buffered when a data channel switched from JSON to raw bytes, so the far end read a header of zeroes | `FramedRead::into_inner()` drops the read buffer; `into_parts()` returns it |

Only a real network could find this. A data channel opens with one JSON line and
is opaque bytes thereafter. On loopback the hello and the USB/IP bytes almost
always arrive in separate reads, so nothing was lost and every loopback test
passed. Across a routed link they coalesce into one segment, and the first
cross-machine claim failed immediately with `usbip version mismatch: peer speaks
0x0000`. The version check earned its keep: the failure was loud and precise
rather than a corrupted stream.

### What the cross-machine test proved

Coordinator and host on one machine with the boards, client and agent on
another, across a routed link (the client arrived from a gateway address, not
its own). Verified:

- an agent on the far machine claimed a board plugged into this one
- it received a **real character device**, backed by its own `vhci_hcd`
- the node was owned by the agent's uid, so unprivileged tools can open it
- a DTR/RTS toggle over the network **reset the chip**, which answered with
  `ESP-ROM:esp32c3-api1-20210207`

That last one is the point of D2. Modem control survives the full round trip:
host → coordinator relay → client → vhci → agent. An `rfc2217` bridge loses
exactly this, and ESP32 auto-reset depends on it.

The relay is the only path: this machine listens on one port (the coordinator),
`usbipd` is not running at all, and the far machine holds no connection here
except to that port.

### Fourth pass: a full adversarial review of the whole system

Four reviewers, one per area. Two of the findings were **privilege escalations**,
and three were introduced by the previous round's fixes.

| Severity | Bug | Root cause |
|---|---|---|
| Critical | A registered bench's `by_id` was never validated, so a bench declaring `/etc/shadow` got it bind-mounted into an agent's sandbox **and chowned to the agent's uid** | The path was validated nowhere; the chown added in pass three turned a read into a write |
| Critical | The containment check was lexical, and the agent owned its own lease directory — plant a symlink at the next lease id and root follows it | `dest.starts_with(root)` before resolution, plus a world-writable `/run/benchd` |
| Blocker | A read *error* on a connection skipped the whole cleanup block, so leases were never released and benches stayed busy for their full TTL | `line?` returned early past `end_session` |
| Blocker | The client's epoch watermark and per-lease maps were never cleared, so a coordinator restart made it fence out legitimate work permanently | Exactly the host bug from pass two, reintroduced when fencing was added to the client |
| Blocker | A refused registration deleted a healthy bench and killed its leases | `drop_bench` ran before validation — from pass three's "replace stale registration" fix |
| High | A lingering host connection's heartbeats were credited to its replacement, hiding a wedged host; its disconnect removed the successor | `HostConn` keyed by bench with no connection identity |
| High | A distinctness conflict against **free** benches was advertised as retryable | `unsatisfiable()` ignored `conflict_only` — the exact spin D14 exists to prevent |
| High | Two slots sharing one bench burned two epochs and emitted two `Export`s, so a relayed claim could never pair | The claim loop iterated assignments rather than distinct benches |
| Medium | Contention ETA ignored a pending teardown, so agents were told to wait ~100× too long after a forced release | `busy()` always quoted `expires_at` |

**Three of these came from the previous round of fixes.** Each pass has
introduced new bugs of the same class as the ones it fixed — the epoch reset
most starkly, fixed on the host and then reintroduced on the client weeks of
work later. The lesson is not "test more" but that a fix to an invariant must be
applied to *every* component that holds it, and the invariant itself written
down somewhere a reviewer can check.

Multi-coordinator support (D21) produced the pattern a third time, and it is
worth recording because the invariant was *brand new*: lease ids had to be
rewritten to their agent-facing form on the way out, which was done on the grant
path and forgotten on the status path, so `lease_status` attributed every lease
to the first coordinator. Newly-introduced invariants are not safer than
long-standing ones — they are more dangerous, because no reviewer has the habit
of checking them yet.

The privileged daemons remain the least-tested surface: both escalations lived
where no test ran.

**The cause was uniform: every test exercised the happy path.** None asked what
happens when a step fails. `tests/failure_paths.rs` exists to keep that from
recurring, and immediately found the `tick` bug.

### Fifth pass: six reviewers, and no design document

Six reviewers, one per area, **deliberately not given this document**. The previous four
passes all reviewed the code against the design, which can only find places where the
code fails to match — never a place where the design is itself wrong. Three of the
findings below are of exactly that kind, and D12 and D22 are now different decisions
because of it.

| Severity | Bug | Root cause |
|---|---|---|
| Critical | Ending a lease unbound the device outright, so `cdc_acm` reclaimed it and a `/dev/ttyUSB0` reappeared on the host — permanently. Every lease after a bench's first silently voided D22 | Exporting owned the stub binding as well as the socket, so releasing a lease undid the hiding |
| Critical | The lease's bind mount shared the source inode, so chowning it to the agent landed on `/dev/ttyACM0` machine-wide, at the agent's uid and its original group | A bind mount of a device file is the same inode; only the path differs |
| Critical | A host could declare `name=<someone else's bench>` and capture every claim aimed at that board | `name` is an open key and unvalidatable by construction; registration never applied the one rule that covers it |
| Blocker | A client could answer an instruction addressed to a host, and a failed export was reported to the agent as the client's | Pending requests were keyed by request id alone, with the replier's role hardcoded per handler |
| Blocker | A host reconnecting destroyed its own bench: the coordinator forgot the displaced connection, then dropped the bench when the survivor disconnected | `HostConn` keyed by bench, so two connections for one bench could not both exist |
| Blocker | A fanout could answer an agent twice, never, or hand back a partial view as if it were the whole lab | No invariant tied a fanout's completion to the number of coordinators it was sent to |
| High | The matcher priced the rarity of a tag *value*, so `flash=4mb` on one board cost twice `flash=8mb` on two — "least capable wins", backwards | The denominator counts exact `key=value`, which is meaningless for an ordinal key; `flash` was weight 1 |
| High | `psram=none` was charged exactly like a real capability, so a board with nothing was priced as though it had something rare | The shipped vocabulary contained absence values, which the matcher cannot detect |
| High | Scarcity was computed over every bench rather than the allocatable ones, pricing the last free JTAG board at half its worth | `tag_counts` ignored `busy` |
| Medium | `install.sh` and `deploy.sh` installed and checksum-verified a binary the build had never written | Both assumed `./target`; `CARGO_TARGET_DIR` in the environment sends the artefact elsewhere, and the scripts then agreed with themselves about the wrong file |

**Not reading the design document was the point.** Every earlier pass had asked "does the
code do what the doc says?", and the answer to that question cannot expose a wrong
decision. D12's weighting rule had been *validated* against the real inventory in an
earlier pass (Appendix C) and was still wrong, because the earlier check only asked
whether rare-versus-valuable had been fixed for the keys it had in mind. A reviewer given
the tag list and no rationale asked what the denominator meant for a key whose values are
ordinal, and there was no good answer.

**The two critical device bugs were both invisible to every existing test, in the same
way.** Nothing had ever ended a lease and then looked at the machine. The host tests
asserted that materialisation worked; none asked what `/dev` contained afterwards, on
either side. That is the same lesson as the fourth pass's "every test exercised the happy
path", one step further along: it is not enough to test that failure paths run, the tests
have to look at the state the system was supposed to leave behind.

**And the deployment scripts were verifying themselves, not the deployment.** The stale
binary was found by hand, when a flag that had just been added reported as unknown on a
freshly deployed daemon whose checksum matched. Both scripts now ask cargo where it built
rather than assuming. The same class of mistake was still present in the test harness a
day later, where it made a merged, working fix report as eight failures.

## Appendix C — corrections made during design

Both were caught by checking, not by thinking harder.

1. **"Multi-slot claims are reachable through labgrid's gRPC API without patching."**
   Wrong — the coordinator's own scheduling loop hardcodes `filters["main"]`, so it
   would have needed a coordinator patch.
2. **Unweighted scarcity scoring** would have made the matcher hoard the lab's cheapest
   board, because rarity stood in for value. Caught by running it against the real
   inventory.
