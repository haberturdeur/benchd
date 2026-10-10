# benchd

`benchd` makes hardware attached to another machine behave as though it were connected locally.

# Quick start

If you are **using** an existing lab:

```sh
sudo benchd client \
  --coordinator lab=lab.example.com:4711

benchd benches
benchd lease soc=esp32s3 -- zsh
```

To keep it running, `sudo systemctl enable --now benchd-clientd` and put the lab address in `/etc/systemd/system/benchd-clientd.service.d/10-coordinator.conf`.

If you use several labs, add another `--coordinator` for each one:

```sh
sudo benchd client \
  --coordinator office=office.example.com:4711 \
  --coordinator ci=ci.example.com:4711
```

Inside the leased shell, use the board through the `$LAB_DUT_*` device paths exactly as if it were connected locally.

The infrastructure behind that consists of:

- **clients**, which make remote hardware local;
- **hosts**, which expose physical hardware;
- **coordinators**, which connect the two and manage leases.

---

# Using benchd as a client

Run the client on the machine where you want to use remote hardware:

```sh
sudo benchd client --coordinator lab=127.0.0.1:4711
```

The client connects to a coordinator and creates local device nodes for any benches you lease. That is what makes remote hardware appear local.

Because it creates device nodes, the client must run as root.

Once the client is running, normal `benchd` commands do not need root.

## Connect to multiple labs

A single client can connect to multiple coordinators at the same time.

Repeat `--coordinator` using a short name for each lab:

```sh
sudo benchd client \
  --coordinator office=127.0.0.1:4711 \
  --coordinator ci=127.0.0.1:5711 \
  --coordinator remote=127.0.0.1:6711
```

You can then discover and lease benches from all connected labs through the same local client.

The coordinator name is local to the client. Choose short names that make it obvious which lab a bench belongs to.

## Client options

The client has no config file. Everything is configured with command-line flags.

### `--coordinator`

```text
--coordinator name=address
```

Adds a coordinator.

This option is repeatable, so one client can connect to several labs.

Default:

```text
local=127.0.0.1:4711
```

### `--root`

```text
--root /run/benchd
```

Controls where active leases appear.

Default:

```text
/run/benchd
```

### `--socket`

```text
--socket /run/benchd/agent.sock
```

Controls the Unix socket used by `benchd` tools and agents.

Default:

```text
/run/benchd/agent.sock
```

---

# Finding and using a bench

Once the client is running, start by listing the hardware available through its connected coordinators.

```sh
benchd benches
```

This shows registered benches and whether they are currently free.

`benchd benches` asks the local client, so it covers every lab that client is connected to.

Filter by the machine hosting the hardware, or combine host and capability tags:

```sh
benchd benches --host lab-a
benchd benches host=lab-a soc=esp32s3
```

Every filter must match. Host names are exact tag values, not substrings. The same
filters work with `benchd --coordinator ADDRESS benches`.

## Lease by capability

Normally you do not need to know which physical board you want.

Request the capabilities you need using tags:

```sh
benchd lease soc=esp32s3 --ttl 30m
```

`benchd` finds a free matching bench, leases it exclusively, and prints the local device paths created for it.

You can specify multiple tags:

```sh
benchd lease soc=esp32s3 jtag=builtin --ttl 30m
```

Tags describe capabilities rather than physical machine names, so the same command can work across multiple labs.

Without `--ttl`, a lease is held for 15 minutes.

## Run a command while holding the lease

The most convenient form is usually:

```sh
benchd lease soc=esp32s3 -- zsh
```

`benchd`:

1. finds a matching free bench;
2. acquires the lease;
3. creates the local device nodes;
4. sets the corresponding `$LAB_DUT_*` environment variables;
5. runs your command;
6. releases the bench when the command exits.

Inside the shell, you can use the hardware normally.

For example:

```sh
esptool.py --port "$LAB_DUT_CONSOLE" ...
```

or:

```sh
idf.py -p "$LAB_DUT_CONSOLE" monitor
```

or:

```sh
minicom -D "$LAB_DUT_CONSOLE"
```

The same pattern works well for test scripts:

```sh
benchd lease soc=esp32s3 -- ./run-hardware-tests.sh
```

When the command exits, the lease is released automatically.

## Lease more than one board

Some tests need two boards that must talk to each other.

Add a named slot for each additional board:

```sh
benchd lease soc=esp32s3 --slot peer:soc=esp32c3 -- ./run-mesh-test.sh
```

All slots are granted together or not at all, and each gets its own environment variables (`$LAB_DUT_CONSOLE`, `$LAB_PEER_CONSOLE`).

You can select a host for each slot:

```sh
benchd lease soc=esp32s3 host=lab-a \
  --slot peer:soc=esp32c3,host=lab-b -- ./run-mesh-test.sh
```

Use the same `host=...` tag on both slots to require two distinct benches on one
host. A host tag in the bare arguments applies only to `dut`; each additional slot
has its own tags.

One coordinator must satisfy the whole request, even if the benches are on
different physical hosts. If a slot is busy, missing, or would exceed the lab's
lease limit, no benches are reserved for that request. If device setup fails, the
whole lease is torn down. Releasing or losing the lease releases every slot.
Requests are not split across coordinators.

## Claim benches from one physical group

A **bench** is one indivisible ownership unit. Keep a board and its wired logic
analyzer in one bench; two boards that can electrically affect each other also
belong in one bench. A **group** connects independently usable benches into a
physical setup. Group members can live on different hosts under one coordinator.

Give each member the same optional `group` in its host config, before any tables:

```toml
id = "esp32s3-a"
group = "radio-setup-a"
tags = ["soc=esp32s3"]
# Existing resource tables follow.
```

Each bench belongs to at most one group. Group names are case-sensitive plain
components (up to 64 ASCII letters, digits, underscores, dots or dashes; neither
`.` nor `..`). Membership is configuration, separate from capability tags and
host names. There are no overlapping or nested groups.

Ask for two independently claimable ESP32 benches in one group:

```sh
benchd lease family=esp32 --slot peer:family=esp32 --grouping same \
  -- ./run-radio-test.sh
```

| Grouping mode | Selection and reservation |
| --- | --- |
| `none` (default) | Select independently across groups or ungrouped benches. |
| `same` | Every slot must be in one nonempty group; reserve selected benches only. |
| `exclusive` | Select from one group and block every member, including unselected members. |

The coordinator chooses a suitable group by capability and best fit; callers do
not need to pick a group by name. Slots use distinct benches by default, so two
slots asking for `family=esp32` require two benches, not two boards wired into one
bench. Existing claims and configurations keep their default behavior.

MCP `claim` accepts the same `grouping` values. A four-role request can express
this composition:

```json
{
  "slots": {
    "dut": ["family=esp32"],
    "peer": ["family=esp32"],
    "wifi": ["device=wifi-adapter"],
    "bluetooth": ["device=bt-adapter"]
  },
  "grouping": "same",
  "ttl_seconds": 600,
  "reason": "radio interoperability test"
}
```

This adapter example describes the claim model. `device` capabilities must be
added to the lab vocabulary; Wi-Fi/Bluetooth adapter resources and native network
tool access are a separate increment. This version implements grouping for the
existing serial and storage resources.

An exclusive claim requires every member to be free, including members already
leased by the requesting session or still releasing. It does not upgrade or merge
existing leases. Only selected devices are exported, and only selected benches
count toward `max_benches`. The whole reservation shares one TTL and lifecycle.
Normal claims can use disjoint members of a group concurrently.

`benchd benches` shows membership and distinguishes a directly held bench from
one blocked by an exclusive group. Grants and lease status include the selected
group and mode. Exclusive reservations cover newly registered members and remain
in force through teardown until acknowledgement or the existing teardown deadline.
Membership changes are refused while the bench is held/releasing or its old or
new group is exclusively reserved. Keep group definitions stable during tests.
A group describes registered hardware; it does not guarantee that every device
normally present in that setup is online, or provide RF isolation from other setups.

No suitable common group is an unsatisfiable request; a suitable group that is
busy is retryable. All slots and the group reservation are admitted atomically
inside one coordinator, never split across deployments.

## Inspect active leases

```sh
benchd leases
```

This shows which benches are currently leased and who holds them.

Unlike `benchd benches`, this asks a coordinator directly, defaulting to `127.0.0.1:4711`.

To inspect a different lab, give its address before the subcommand:

```sh
benchd --coordinator lab.example.com:4711 leases
```

## Release a bench manually

```sh
benchd release --bench <bench-id>
```

For example:

```sh
benchd release --bench esp32s3-a
```

This also talks to a coordinator, so the same `--coordinator` rule applies.

You normally do not need this when using:

```sh
benchd lease ... -- <command>
```

because that form releases the lease automatically.

---

# Connecting to remote labs

The client only needs to be able to reach each coordinator.

If a coordinator is not directly reachable, forward its port over SSH and configure the client to connect to the local end of the tunnel.

For example, if an SSH tunnel exposes a remote coordinator at:

```text
127.0.0.1:4711
```

use:

```sh
sudo benchd client \
  --coordinator lab=127.0.0.1:4711
```

You can combine tunneled and directly reachable coordinators:

```sh
sudo benchd client \
  --coordinator local=10.0.0.20:4711 \
  --coordinator remote=127.0.0.1:5711
```

A worked SSH configuration is available in:

```text
examples/tunnel.conf
```

---

# Using benchd with coding agents

Agents can use the same client and acquire and release hardware leases themselves.

The supplied plugin contains:

- a skill explaining how to select and lease benches;
- an MCP server that performs the bench operations.

## Codex

```sh
codex plugin marketplace add .
```

Then enable the `benchd` plugin.

## GitHub Copilot

```sh
copilot plugin install ./plugins/benchd
```

## Cursor Agent

```sh
cursor-agent --plugin-dir ./plugins/benchd
```

## Other MCP-capable agents

Copy:

```text
plugins/benchd/skills/benchd/SKILL.md
```

into the agent's skills directory.

Configure this MCP server:

```sh
/usr/local/bin/benchd mcp
```

Give the agent a short, stable identity:

```sh
export BENCHD_IDENTITY=codex-1
```

The identity appears in lease and contention information, making it possible to see which user or agent currently holds a bench.

Once configured, the agent can claim and release benches itself.

---

# Isolating multiple agents

You only need additional isolation when:

- several agents share the same Unix user; and
- those agents should not be able to access one another's leased devices.

Give each agent its own identity and launch it through `benchd-sandbox`:

```sh
benchd-sandbox --identity codex-1 -- codex
```

```sh
benchd-sandbox --identity cursor-1 -- cursor-agent
```

Each sandbox gets:

- a `/dev` containing only devices belonging to that agent's leases;
- a read-only view of the host filesystem;
- a writable project directory.

The wrapper also handles interaction with each agent CLI's own sandbox.

Requirements:

```text
bubblewrap
benchd
```

If agents already run as separate Unix users, this extra isolation may not be necessary.

---

# How a benchd lab works

You do not need to run the lab infrastructure just to use a bench, but it helps to understand the three roles.


| Role            | Usually runs where                     | Needs root? | Purpose                                    |
| --------------- | -------------------------------------- | -----------: | ------------------------------------------ |
| **Client**      | Developer or agent machine             | Yes         | Makes leased remote devices appear locally |
| **Host**        | Machine physically connected to boards | Yes         | Exposes a physical bench                   |
| **Coordinator** | One machine per lab                    | No          | Tracks benches, tags, leases, and limits   |


These roles are independent. A machine can run any combination of them.

The connection direction is:

```text
                 ┌─────────────────┐
                 │   Coordinator   │
                 │      :4711      │
                 └────────┬────────┘
                          │
              ┌───────────┴───────────┐
              │                       │
        connects to             connects to
              │                       │
      ┌───────┴───────┐       ┌───────┴───────┐
      │      Host     │       │     Client    │
      │  physical HW  │       │  local tools  │
      └───────────────┘       └───────────────┘
```

Both hosts and clients initiate connections to the coordinator.

The coordinator never touches hardware directly.

---

# Operating a bench

This section is for people setting up the physical hardware.

Run one `benchd host` process for each physical setup:

```sh
sudo benchd host \
  --config /etc/benchd/benches/esp32s3-a.toml \
  --coordinator 127.0.0.1:4711
```

The host:

- connects to the coordinator;
- registers the bench and its capabilities;
- exposes its devices to lease holders;
- hides those devices from the rest of the host machine while it runs.

Because it manages hardware devices, the host requires root.

## Bench configuration

The configuration file defines the physical bench.

It contains:

- an `id`;
- a free-form `description`;
- capability `tags`;
- hardware `resources`;
- optionally `docs`, notes handed only to whoever holds the bench.

For example:

```toml
id          = "esp32s3-a"
description = "ESP32-S3 devkit, native USB-JTAG"
tags        = ["soc=esp32s3", "jtag=builtin", "console=usbjtag"]

[resources.console]
kind  = "serial"
by_id = "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_..."
```

Every bare key must come before the first table, or TOML folds it into that table.

Start from one of:

```text
examples/bench-*.toml
```

## Use stable device identifiers

Configure resources using stable identifiers such as `by_id` or `by_path`.

Do not depend on transient names such as:

```text
/dev/ttyUSB0
```

Those names can change after unplugging and reconnecting hardware.

Stable identifiers let the same physical device be rediscovered reliably.

## Tags must exist in the coordinator vocabulary

Every tag a bench declares must be defined by that coordinator.

For example:

```toml
tags = ["soc=esp32s3", "jtag=builtin"]
```

requires the coordinator to know the `soc` and `jtag` tag keys, and those values.

A coordinator rejects a bench containing unknown tags.

### Host labels

`host` is a built-in open tag key: its values need no coordinator vocabulary
entry. Each `benchd host` automatically advertises `host=<hostname>`, using the
machine's kernel hostname converted to lowercase. To use a stable label instead,
include it in the bench's existing tags:

```toml
tags = ["soc=esp32s3", "jtag=builtin", "host=lab-a"]
```

An explicit host tag replaces the automatic hostname tag. Values follow the
usual tag syntax (lowercase letters, digits, dots, underscores, and dashes).
Use the same label in every bench config on a machine if they should be selected
together. Host labels appear in `benchd benches` and MCP `tag_list`, and can be
used in CLI or MCP claim slots.

Upgrade all components together when the protocol number changes (see protocol
compatibility below). Hosts advertise their automatic tag after restarting.

---

# Operating a coordinator

Each lab has one coordinator.

Start it with:

```sh
benchd coordinator \
  --listen 127.0.0.1:4711 \
  --config /etc/benchd/coordinator.toml
```

The coordinator does not access hardware and does not require root.

Hosts and clients connect to it.

## Coordinator configuration

The coordinator config defines the lab's policy and vocabulary.

It contains:

- which tag keys exist;
- what those tags mean;
- relationships where one tag implies another;
- lease limits.

Start from:

```text
examples/coordinator.toml
```

The coordinator config deliberately contains **no list of benches**.

Benches register dynamically when their `benchd host` processes start.

## Lease limits

The `[limits]` section controls policies such as:

- the maximum duration of a lease;
- the total time one holder may keep renewing a bench;
- the maximum number of benches one holder can lease simultaneously.

These limits apply across the coordinator's lab.

---

# Running benchd with systemd

`dist/install.sh` installs systemd units corresponding to the normal command-line roles:


| Unit                  | Purpose                                 |
| --------------------- | --------------------------------------- |
| `benchd-clientd`      | Run the client                          |
| `benchd-coordinator`  | Run a lab coordinator                   |
| `benchd-host@<bench>` | Run one physical bench                  |
| `benchd-tunnel@<lab>` | Maintain an SSH tunnel to a coordinator |


Enable only the units needed by that machine.

A developer workstation might run:

```text
benchd-clientd
benchd-tunnel@remote-lab
```

A hardware server might run:

```text
benchd-host@esp32s3-a
benchd-host@stm32-a
```

A small lab server may run both coordinator and host services.

Coordinator addresses used by systemd units live in drop-ins under:

```text
/etc/systemd/system/<unit>.d/10-coordinator.conf
```

A tunnel's target and key live in:

```text
/etc/benchd/tunnels/<lab>.conf
```

## Automatically start connected benches

Place bench configurations in:

```text
/etc/benchd/benches/
```

Then run:

```sh
sudo benchd update-benches
```

`benchd` checks which configured hardware is actually connected and enables the corresponding:

```text
benchd-host@<bench>
```

unit, disabling the units whose hardware is absent.

This allows a hardware machine to contain configurations for several benches without starting host processes for hardware that is currently unplugged.

---

# Common deployment layouts

## Developer using an existing lab

This is the normal case.

Your machine runs:

```text
Client
```

The lab already provides:

```text
Coordinator
Host(s)
```

Your client can connect to one lab:

```sh
sudo benchd client \
  --coordinator lab=lab.example.com:4711
```

or several:

```sh
sudo benchd client \
  --coordinator office=office.example.com:4711 \
  --coordinator ci=ci.example.com:4711
```

---

## One-machine development lab

Everything runs on the same machine:

```text
Coordinator
Host
Client
```

Use:

```text
127.0.0.1:4711
```

for the coordinator address.

---

## Shared hardware server

Hardware server:

```text
Coordinator
Host(s)
```

Developer machines:

```text
Client
```

Each developer client connects to the shared coordinator.

---

## Dedicated coordinator

Coordinator machine:

```text
Coordinator
```

Hardware machines:

```text
Host(s)
```

Developer machines:

```text
Client
```

Both hosts and clients connect to the coordinator.

A client may also connect to coordinators belonging to other labs at the same time.

---

## Coordinator reachable only over SSH

Remote lab:

```text
Coordinator
Host(s)
```

Developer workstation:

```text
SSH tunnel
Client
```

Point the client at the local end of the SSH tunnel.

Multiple tunnels can be used if the same client needs access to several remote labs.

---

# Troubleshooting

## Incompatible deployments

Every connection between benchd components starts with a protocol compatibility
handshake, including CLI/MCP connections to the local client daemon and USB/IP
relay connections. Peers exchange a protocol number, package version and build
identifier. Different builds can communicate when their protocol numbers match;
different protocol numbers are refused before registration, sessions or claims.

Grouping uses protocol **2**. Protocol 1 components are rejected, preventing an
older coordinator from silently ignoring a group constraint. Upgrade and restart
the coordinator, hosts, client daemons and CLI/MCP processes together.

Use `benchd --version` on each machine to see all three values. A mismatch error
reports both peers' details. Hosts and client daemons also log the error in their
service journals. The build identifier defaults to the Git revision, with
`-dirty` for tracked working-tree changes; builds from source archives report
`unknown` unless built with `BENCHD_BUILD_ID=<release-or-ci-build-id>`.

The first version introducing this handshake cannot communicate with older,
unversioned binaries. Upgrade the coordinator, hosts, client daemons, CLI/MCP
processes and `benchd-sandbox` together, then restart them. Existing leases are
lost when their coordinator or client daemon restarts. Older peers are rejected
with an upgrade message where their protocol supports it; peers that do not
answer the hello hit a five-second handshake deadline.

For maintainers: increment `PROTOCOL_VERSION` in `benchd-core/src/protocol.rs`
whenever a wire-format or behavioral change makes peers incompatible. A build
identifier helps diagnose a deployment; it is not proof of compatibility or
peer authentication.

## No benches appear

Run:

```sh
benchd benches
```

If it cannot reach the client daemon, `benchd client` is not running on this machine.

If nothing is listed, check that the client is connected to the expected coordinator or coordinators.

For a multi-lab client, verify each `--coordinator name=address` entry independently.

---

## A lease cannot be acquired

Check:

```sh
benchd benches
benchd leases
```

Possible causes include:

- all matching benches are already leased;
- no bench matches the requested tags;
- the coordinator's lease limits would be exceeded.

---

## Tools cannot see the leased device

Verify that:

1. `benchd client` is running;
2. the client was started as root;
3. the lease is still active;
4. your tool is using the path printed by `benchd lease` or the corresponding `$LAB_DUT_*` variable.

Leased devices appear only under the lease path. `/dev/ttyUSB0` and `/dev/ttyACM0` are not yours even when they exist.

---

## A bench is rejected when its host starts

Check its tags.

Every entry in:

```toml
tags = [...]
```

must exist in that coordinator's tag vocabulary.

---

## A device changes name after reconnecting it

Do not configure resources using transient device names such as:

```text
/dev/ttyUSB0
```

Use stable `by_id` or `by_path` identifiers instead.

---

# Command reference

```sh
# Client: one lab
sudo benchd client \
  --coordinator lab=127.0.0.1:4711

# Client: several labs
sudo benchd client \
  --coordinator office=10.0.0.20:4711 \
  --coordinator ci=10.0.1.20:4711

# List available benches
benchd benches

# Lease a bench
benchd lease soc=esp32s3 --ttl 30m

# Run a command while holding a bench
benchd lease soc=esp32s3 -- zsh

# Show active leases
benchd leases

# Release a bench manually
benchd release --bench esp32s3-a

# Physical bench host
sudo benchd host \
  --config /etc/benchd/benches/esp32s3-a.toml \
  --coordinator 127.0.0.1:4711

# Coordinator
benchd coordinator \
  --listen 127.0.0.1:4711 \
  --config /etc/benchd/coordinator.toml

# Enable hosts for configured hardware that is currently connected
sudo benchd update-benches
```

---

# Source

The decisions behind this, and the alternatives that were rejected, are in
[`docs/design.md`](docs/design.md).

```sh
dist/install.sh          # first time: the binary, config, systemd units
dist/deploy.sh           # thereafter: rebuild, install, verify checksums
dist/cross-build.sh      # for a machine that has no Rust toolchain of its own
cargo test
```
