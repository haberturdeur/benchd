# benchd — design

**Status:** design settled, partially implemented. Matcher done and tested; policy,
leases, materializer and MCP surface are specified here but not written.

**Audience:** you (Tom), future-you, and agents asked to modify this. When code and
this document disagree, **this document wins** — fix the code or fix the doc, but
don't leave them in conflict.

---

## 1. Problem

One machine, many ESP32 boards, several AI agents working concurrently. Agent A
starts a test on a board; agent B flashes the same board mid-run. Both results are
garbage, and the failure is silent — the expensive kind to debug.

We need agents to take exclusive, time-bounded custody of hardware, and we need
that custody to be **real** rather than advisory.

## 2. Goals

1. **Agents claim hardware by capability, not by name.** `claim(soc=esp32s3)`, not
   `claim(esp32s3-a)`. Re-cabling the lab must not break agent scripts.
2. **Normal tools work.** After a claim, `esptool`, `idf.py monitor`, `minicom` and
   `openocd` operate on a real character device. No wrapper, no URL scheme, no
   MCP-mediated console.
3. **Claims are enforced, not advisory.** Without a lease the device is *not
   reachable*. "Agent forgot to claim" is `ENOENT`, not corruption of someone
   else's run.
4. **Claims expire.** Every claim carries a mandatory, explicit TTL. Agents crash,
   wander off, and hoard; hardware must come back without human intervention.
5. **Failures are diagnosable by the agent itself.** "No bench will ever match
   this" and "they're all busy right now" are structurally different answers.

## 3. Non-goals

- **Authentication.** Identity is self-asserted. This is *policy*, not security —
  see §9.
- **A test framework.** benchd hands out hardware. What you do with it is your
  business.
- **Batch job scheduling.** Explicitly rejected; see D1.
- **Multi-machine, for v1.** Designed for, not built yet; see D6.
- **Power/USB-mux/SD-mux control, for v1.** Extension point exists; see D5.

---

## 4. Concepts

| Concept | Definition |
|---|---|
| **Resource** | One physical thing: a serial device (by stable `/dev/serial/by-id` path), later a USB device for USB/IP, a relay, a probe. |
| **Bench** | The unit of exclusion. A named set of resources that must be held together — the board *and* its USB-JTAG interface *and* the relay that power-cycles it. |
| **Tag** | A `key=value` capability fact about a bench. `soc=esp32s3`, `psram=octal`. |
| **Slot** | A named role within a claim: `dut`, `peer`. Each slot carries a requirement. |
| **Claim** | A request for one or more slots, satisfied atomically or not at all. |
| **Lease** | A granted claim: which benches, held by whom, until when. |

A bench is the atom because a board rarely arrives alone. If two boards share an RF
chamber, they are one bench — or the chamber is its own bench and you use
anti-affinity.

---

## 5. Architecture

```
    agent (pi / claude / cursor)
        │  MCP: tag_list, claim, renew, release, lease_status
        ▼
  ┌──────────────────────────────────────────┐
  │ benchd (single process, runs as root)    │
  │                                          │
  │  matcher ── policy ── lease manager      │  ← single writer, one lock
  │                           │              │
  │                      materializer        │
  └───────────────────────────┼──────────────┘
                              ▼
        /run/benchd/agents/<owner>/<lease-id>/<slot>/<resource>
                              │  bind mount
                              ▼
                        /dev/ttyACM0
```

**Single writer is the core invariant.** Nothing but benchd acquires or releases a
bench. Claim, renew, expiry and preemption are all serialised inside one process
under one lock, so the classic read-then-force-release race cannot occur by
construction. This is why the reaper is in-process rather than a separate service.

---

## 6. Design decisions

Each entry records what we chose, why, and what we rejected. The rejected options
are the valuable part — they're what stops us relitigating this in six months.

### D1. Leases, not CI jobs

**Decision:** model hardware access as a *lease* (mutual exclusion over a bounded
window, held across many independent commands) rather than a *job*.

**Why:** hardware work with an agent is iterative — flash, poke, read, tweak,
reflash. Wrapping each step in a job submission turns a seconds-long loop into a
minutes-long one, and agents compensate badly (over-batching, guessing instead of
measuring). CI also doesn't actually solve the problem: unless *all* access goes
through it, a shell still owns `/dev/ttyUSB0`.

**Rejected:** GitHub Actions self-hosted runners with concurrency groups; LAVA.

### D2. Enforcement, not advisory locking

**Decision:** the device node does not exist unless you hold a lease.

**Why:** advisory locks work on humans because social pressure covers the gap. With
agents there is no social pressure. An agent that forgets, or that saw
`esptool --port /dev/ttyUSB0` in a README, or that decides a lock is "probably
stale", silently corrupts someone else's run. Make the lease load-bearing and that
whole class disappears.

### D3. Real device nodes via bind mounts

**Decision:** benchd bind-mounts the real device inode into a per-lease directory.

**Why:** Goal 2 rules out every alternative. `rfc2217://` and `socket://` are
pyserial-only conveniences — `idf.py monitor`, `minicom` and `openocd` all refuse
them. A pty bridge loses modem-control lines, which breaks ESP32 auto-reset (the
DTR/RTS dance into the bootloader) in a way that is intermittent and miserable to
debug. Symlinks dangle inside a sandbox that has no `/dev`. A bind mount puts the
actual inode at the destination path and behaves exactly like the device, because
it *is* the device.

**Cost:** benchd must run as root (`CAP_SYS_ADMIN`). The agent needs no privileges
at all, which is the point.

### D4. Per-lease paths, never stable ones

**Decision:** devices appear at `/run/benchd/agents/<owner>/<lease-id>/<slot>/<res>`,
surfaced as `$LAB_DUT_CONSOLE`.

**Why:** if `/dev/lab/dut` meant board A last lease and board B this lease, a stale
shell or backgrounded script writes to the wrong board — reintroducing the exact
failure this system exists to prevent, through the back door. With the lease id in
the path, a stale reference fails `ENOENT`.

**Rejected:** literal `/dev/ttyUSB0`. Kernel indices renumber on replug and collide
between boards, which reintroduces the identity problem tags were supposed to
remove.

### D5. Greenfield, with labgrid available as a library later

**Decision:** don't fork labgrid, don't merge into it, don't depend on it.

**Why:** the piece we need most — a real device node materialising in a client-side
sandbox — fits labgrid's architecture worst. Its model is "the resource stays on the
exporter; the client talks to it over the network through a Driver". A root-side
subsystem that bind-mounts inodes is none of Resource/Driver/Exporter. Meanwhile the
parts we'd have to modify are `remote/` (6.5k LOC — coordinator 1,246, client
2,585), the highest-coupling area, and we'd inherit 9.6k LOC of drivers we currently
need zero of.

**The dilemma dissolves** because labgrid's drivers don't depend on its coordinator
(verified: `driver/powerdriver.py` imports only `..resource`, `..protocol`,
`..step`, `..util`). The day a PDU or SD-mux arrives, `pip install labgrid` and use
the driver against a locally-declared resource — no fork, no coordinator.

**What we gave up:** multi-machine transport, udev auto-discovery
(`resource/udev.py`, ~200 lines of pyudev to redo), and the pytest plugin.

**Risk:** at six machines with PDUs and muxes we'll have rebuilt part of labgrid,
worse. Mitigated by keeping `Resource` an interface and keeping matcher/policy/leases
ignorant of transport, so a labgrid-backed bench source can be added without
touching them.

**Facts checked (2026-08, labgrid v26.0):** tag matching is `f.tags.issubset(place.tags)`
in `remote/scheduler.py` — exactly the semantic we want, and its allocator is
contention-aware in a way our first design wasn't. But: acquired places **never
expire** (`coordinator.py` carries a `FIXME` asking for exactly that), multi-group
reservations exist in the scheduler yet both `client.py:1581` and
`coordinator.py:1031` hardcode `filters["main"]`, and there is **no USB/IP support
anywhere** in code, docs or issues.

### D6. Local first, remote behind a trait

**Decision:** v1 is one machine. `Materializer` is a trait; the remote
implementation is USB/IP.

**Why USB/IP when we get there:** it is the only mechanism that yields a *genuine*
`/dev/ttyUSB0` remotely, because the client's own kernel binds `cp210x`/`ch341`/
`ftdi_sio` to the forwarded device. Full termios, real DTR/RTS, so auto-reset works.
Revocation is kernel-level: `usbip unbind` makes the device vanish mid-operation.

**Known costs:** the protocol is unauthenticated cleartext TCP and **must** be
tunnelled (SSH/WireGuard, never port 3240 exposed); both ends need kernel modules;
`attach` needs root on the client, so benchd's agent-side helper does it.

### D7. Tag vocabulary is closed, `key=value`, with implications

**Decision:** tags are `key=value` from a closed vocabulary; benches expand through
an implication graph; unknown tags are rejected with did-you-mean suggestions.

**Why:** free-form tags rot into `esp32-s3` / `esp32s3` / `s3` within a week once
*agents* are writing the requests. A closed vocabulary turns a typo into a
self-correcting error in one turn instead of ten turns of "no bench matches".
Implications (`soc=esp32s3` ⇒ `family=esp32`, `arch=xtensa`, `jtag=builtin`) mean
agents don't need to know the hardware taxonomy; the inventory does.

**Asymmetry, deliberate:** *bench* tags are expanded, *requirements* are never
expanded. Expanding a request would make it strictly harder to satisfy — the
opposite of the intent.

**Dashes:** legal in tag values generally (identity values like `name=esp32s3-a`
need them), rejected in *vocabulary* values where capabilities are declared. That's
the layer where the `esp32-s3`/`esp32s3` split actually happens.

### D8. Best fit, not first fit — and weighted

**Decision:** among adequate benches, pick the least capable one. Cost of a bench
for a requirement is `Σ weight(key) / (benches carrying that tag)` over the tags it
would waste.

**Why:** first-fit hands the lab's only JTAG bench to an agent that asked for a
blinking LED, and ten minutes later the agent that needs JTAG blocks.

**Why weighted — a bug we actually hit:** pure scarcity conflates *rare* with
*valuable*. Being the only CP2102N board made `usb=cp2102n` unique, so the matcher
protected the cheapest board in the lab and handed out the PSRAM one instead.
Identity keys (`soc`, `arch`, `usb`, `family`) get weight 0; contended peripherals
(`jtag`, `psram`, `chamber`) keep weight 1. Pinned by
`identity_tags_do_not_make_a_cheap_bench_look_precious`.

### D9. Atomic multi-slot claims, solved exactly

**Decision:** a claim names slots (`dut`, `peer`), all satisfied or none. Assignment
is exact (branch-and-bound), not greedy.

**Why atomic:** two agents each holding half of what they need is a deadlock.

**Why exact:** greedy per-slot assignment can report failure for a request that *is*
satisfiable, and a false "no bench available" is the worst possible answer here —
it's indistinguishable from real contention and agents will wait forever. Pinned by
the property test against a brute-force oracle.

### D10. Unsatisfiable vs contended is a type, not a string

**Decision:** failures are `Failure::Unsatisfiable | Failure::Contended`, and
diagnosis reports the near misses.

**Why:** these demand opposite agent behaviour. *Unsatisfiable* → change the
request, retrying never helps. *Contended* → wait. Collapsing them into "no bench
available" makes agents retry-spin on impossible requests forever.

Near-miss output lets the agent relax its own request without guessing:

```
slot 'dut': no bench exists matching {psram=octal soc=esp32c3}
  no bench has: soc=esp32c3
  drop soc=esp32c3 -> matches {psram=octal}
```

### D11. Mandatory explicit TTL; renewal is an explicit call

**Decision:** every claim states a duration. There is no default. Renewal is an
explicit `renew` call.

**Why explicit:** naming a duration forces the agent to scope the work, and gives us
requested-vs-used telemetry to tune limits from data rather than guesswork.

**Why not auto-keepalive:** an automatic keepalive recreates the never-expiring hold
we're trying to eliminate. An explicit renew is a *liveness proof* — the agent is
alive **and** still believes it's working.

**Two limits, not one:** `max_ttl` bounds a single grant; `max_total_hold` bounds
the sum across renewals so an agent can't renew forever and starve everyone. Plus
`max_benches` per identity, or one agent claims four boards for a two-board test.

### D12. Reaping lives in benchd, not upstream

**Decision:** expiry is benchd's job.

**Why:** "reaping" conflates two failure modes. *(a) The holder died* — labgrid's
own FIXME describes the fix (bind acquisition to the client session), and that's
genuinely upstreamable. *(b) The holder is alive but shouldn't keep it* — needs a
deadline, and the policy around it (per-class limits, human-vs-agent, preemption)
cannot go upstream because labgrid has no identity model at all. For agents, (b) is
the common case: the MCP server process stays happily connected long after the agent
lost interest in the board.

### D13. Human preemption; agents never preempt

**Decision:** humans may take a bench from a holder. Agents may not, ever.

**Why:** this is the actual payoff of the human/agent split — not longer timeouts.
When you're at the bench and an agent holds the board you need, you take it back.

**Grace window:** preemption and ordinary expiry both mark the lease `revoking`,
publish that in `lease_status`, wait ~30s, then unmaterialize. Yanking a device
mid-flash can leave a board in a bootloader state.

### D14. Minimal tool surface

**Decision:** agents get exactly `tag_list`, `claim`, `renew`, `release`,
`lease_status`. No `bench_list`, no claim-by-name, no console/flash/power tools.

**Why:** tool surface *is* policy. If a console-read tool exists, an agent will use
it instead of `idf.py monitor`, and you get two divergent access paths with split
logs. If claim-by-name exists, an agent will hardcode a bench name into a test
script and reintroduce the contention this system removes. Claim-by-name and
bench listing stay in the human CLI.

### D15. Rust, TOML

**Decision:** Rust 2021, `rmcp` for MCP, TOML for config.

**Why Rust:** it's a root daemon calling `mount(2)` on paths derived from
agent-supplied strings — the worst possible place for a memory-safety bug. More
specifically, the correctness story rests on single-writer lease state, and Rust
makes "who may mutate the lease table" a compile-time property rather than a
code-review convention. The domain is algebraic (`Held | Revoking | Expired`,
`Unsatisfiable | Contended`) and sum types model it natively. One static binary with
no runtime survives `pacman -Syu`.

**Rejected:** C++ (dependency management, and the safety argument bites hardest
exactly here); Go (genuinely viable — better ergonomics, `syscall.Mount` in stdlib —
but no sum types and data-race prevention drops to `go test -race`); Haskell (the
domain fits beautifully and STM would be ideal, but no MCP SDK, thin udev/serial
ecosystem, and it blunts the agent-assisted debugging this project explicitly relies
on).

**TOML because** `serde_yaml` is `0.9.34+deprecated` and unmaintained since 2024-03.

**Verified:** `rmcp` 3.2.0 (official `modelcontextprotocol/rust-sdk`), MSRV 1.88 —
matches the installed toolchain exactly.

---

## 7. Component specifications

### 7.1 Matcher — *implemented*

`allocate(request, benches, busy, tag_counts, weights) -> Result<Allocation, NoMatch>`

- match = `requirement.tags ⊆ bench.tags`
- among free candidates, minimise total `fit_cost` (D8)
- assignment is exact under the distinctness constraint (D9)
- on failure, per-slot `Unsatisfiable | Contended` diagnosis with near misses (D10)

**Invariants** (pinned by `properties::allocation_agrees_with_brute_force`):
succeeds ⟺ a valid assignment exists; returned cost is optimal; returned assignment
is valid (every slot matches, distinctness holds).

### 7.2 Policy — *specified, prototyped in Python, not ported*

Identity → class → limits. Unknown identities fall back to the **most restricted**
class, so a typo in an agent name can never grant human privileges.

```
agent:  ttl≤15m   total≤2h    benches≤2   renewable
human:  ttl≤8h    total ∞     benches ∞   renewable, may preempt, may claim by name
ci:     ttl≤45m   total≤45m   benches≤4   NOT renewable (fail fast)
```

Over-long TTL is **clamped with a message**, not rejected — an agent asking for 4h
and getting 15m can get on with its work. Every other limit is a hard error with a
message saying what to do instead.

### 7.3 Lease lifecycle — *specified, not written*

```
        claim
          │
          ▼
       ┌──────┐  renew (explicit, bounded by max_total_hold)
       │ HELD │◀─────┐
       └──┬───┘──────┘
          │ T-grace, or preempt
          ▼
     ┌──────────┐  release
     │ REVOKING │──────────┐
     └────┬─────┘          │
          │ grace elapsed  │
          ▼                ▼
      ┌─────────┐   ┌──────────┐
      │ EXPIRED │   │ RELEASED │
      └─────────┘   └──────────┘
```

Every response from every tool carries `expires_at` and `remaining`. Agents plan
terribly against invisible deadlines; "you have 3 minutes left" in a renew response
is the cheapest possible nudge.

### 7.4 Materializer — *trait specified, backends not written*

```rust
trait Materializer {
    fn materialize(&self, lease: &LeaseId, owner: &str, slots: &Slots) -> Result<MaterializedLease>;
    fn unmaterialize(&self, lease: &LeaseId, owner: &str) -> Result<()>;
}
```

Implementations must be **idempotent** — the reaper will sometimes race a voluntary
release and neither path may fail. `materialize` resolves everything before mounting
anything and rolls back on error; a half-materialised lease is worse than a failed
one.

Backends: `Fake` (tests, no privileges), `BindMount` (v1, local), `UsbIp` (later,
remote).

### 7.5 MCP surface — *not written*

`tag_list` returns, per tag: prose description, how many benches carry it, how many
are **free right now**, and typical wait. Agents plan far better against
availability than against a bare list, and it lets them relax a request *before*
claiming.

---

## 8. What agents see when things go wrong

This is a contract, and the skill must state it:

| Situation | Agent sees | Correct response |
|---|---|---|
| Unknown tag | `unknown tag soc=esp32s4 (did you mean: soc=esp32s3?)` | fix and retry immediately |
| No such bench | `Unsatisfiable` + which tags to drop | change the request; never retry as-is |
| All busy | `Contended` + holders + ETA | wait and retry |
| Over TTL limit | clamped, with a message | proceed with the shorter lease |
| Lease expired mid-use | `ENOENT` / `EIO` on the device | **your lease ended** — reclaim; do *not* power-cycle |

That last row matters: without it agents will interpret revocation as broken
hardware and start power-cycling boards to "fix" an expired lease.

---

## 9. Security posture

**benchd enforces policy, not security.** Identity is self-asserted — an agent
claiming to be `tom` gets human limits. This stops honest mistakes and runaway
agents. It stops nothing that is trying.

For a single-user lab this is the correct trade. If it ever needs to be a boundary,
it goes in the transport (per-identity SSH or mTLS), not in the broker. Do not
retrofit trust into the identity string.

The privileged surface is small and should stay small: benchd runs as root and
mounts device inodes into per-owner directories. It never executes agent-supplied
strings, and paths are constructed from validated identifiers only.

---

## 10. Open questions

**Q1 — What are the agent sandboxes?** *(blocking the materializer)*
bubblewrap, Docker/Podman, systemd-nspawn, VMs, or separate unix users? Determines
whether delivery is a directory bind-mount visible live in a running sandbox, a
`--device` + cgroup update, or plain ACLs. **Working assumption:** bubblewrap with
`/run/benchd/agents/<owner>` bind-mounted at `/dev/lab` at sandbox start. Wrong guess
costs about an hour, not a rewrite.

**Q2 — Do leases survive a benchd restart?** Bind mounts are kernel state and will
outlive the process, so a naive restart orphans them and forgets who holds what.
Options: persist leases (sqlite/JSON) and reconcile mounts on startup, or
unmaterialize everything at startup and force re-claims. Leaning reconcile, since a
restart shouldn't kill in-flight agent work. **Not yet designed.**

**Q3 — Device disappears mid-lease** (cable knocked, board resets into a state that
re-enumerates). Does the lease survive and re-materialize, or fail? Leaning: keep the
lease, surface `degraded` in `lease_status`, let the agent decide.

**Q4 — Idle release.** TTL catches crashes; it doesn't catch an agent that claims a
board then spends 12 minutes reading source. Detect via no open fd on the node for N
minutes. Biggest utilisation win once there are more than a couple of agents. v2.

**Q5 — Sticky reclaim.** After release, a short soft-reservation window so the
flash→test→tweak→reflash loop stays on one board. Best-effort, falls back to normal
matching. v2.

**Q6 — Repo layout.** *Resolved 2026-09.* The Rust tree is the repository root and
the Python prototype has been removed; it survives in git history at commit
`08d6bf8` if a design decision ever needs archaeology.

---

## 11. Status

| Component | State |
|---|---|
| Tag model, vocabulary, implications | done, tested |
| Inventory + TOML config | done, tested |
| Matcher (match, best-fit, multi-slot, diagnosis) | done, 19 tests + property test |
| Policy engine | Python prototype only |
| Lease manager + reaper | specified here |
| Materializer (bind-mount) | Python prototype only; blocked on Q1 |
| MCP server | not started |
| Skill | not started |
| USB/IP backend | deferred |

**Suggested order:** close Q1 and Q2 → policy → leases → materializer → MCP → skill.

---

## Appendix — corrections made during design

Recorded because they show where the reasoning was weak, and both were caught by
checking rather than by thinking harder.

1. **"Multi-slot claims are reachable through labgrid's gRPC API without patching."**
   Wrong. The reservation *scheduler* supports named filter groups, but the
   coordinator's own scheduling loop hardcodes `filters["main"]`
   (`coordinator.py:1031`), so it would have needed a coordinator patch after all.
   Caught by reading the source.

2. **Unweighted scarcity scoring.** Would have made the matcher hoard the cheapest
   board in the lab, because rarity was standing in for value. Caught by running the
   code against the real inventory.
