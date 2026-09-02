---
name: benchd
description: Claim physical hardware (ESP32 boards and other dev boards) before flashing, monitoring, or testing on it. Use whenever a task involves a real board — esptool, idf.py, minicom, openocd, serial consoles, or any "flash it and watch the output" work. Boards are shared with other agents, so they must be claimed first.
---

# Claiming hardware with benchd

The boards on this machine are shared with other agents. Before you touch one you
claim it; when you are done you release it. A claim gives you a **real device
node**, so your normal tools work unchanged.

**Never use `/dev/ttyUSB*` or `/dev/ttyACM*` directly.** Those paths are not yours,
they renumber when boards are replugged, and writing to one mid-test corrupts
someone else's run — silently, which is the expensive kind of bug. Use the path
`claim` gives you.

On a correctly set up machine you will not find them anyway: lab boards are not
present in your `/dev` at all until you hold a lease. If `ls /dev/ttyACM0` says
it does not exist, that is not a broken board — it is a board you have not
claimed.

## The loop

```
tag_list                → what hardware exists, and what is free right now
claim {...} ttl_seconds → returns a lease id and a device path per resource
  ... your normal tools, using that path ...
release <lease>         → give it back
```

### 1. Find out what exists

Call `tag_list` first. It returns capability tags with how many benches have each
and how many are free:

```
soc=esp32s3    3 benches, 1 free    "ESP32-S3, Xtensa dual-core with native USB"
psram=octal    1 bench,  1 free     "Octal-SPI PSRAM"
jtag=builtin   3 benches, 1 free    "USB-JTAG built into the SoC"
```

If what you need is scarce, that is worth knowing before you ask for it.

### 2. Claim what you need, and no more

```json
claim {
  "slots": {"dut": ["soc=esp32s3"]},
  "ttl_seconds": 900,
  "reason": "wifi reconnect regression"
}
```

**Ask for the least you actually need.** A bench matches when it has *at least*
the tags you asked for, so `soc=esp32s3` will happily be satisfied by the one
board that also has PSRAM and an RF chamber — and then the agent that genuinely
needs those waits. Only list a capability if your test would fail without it.

**`ttl_seconds` is required, and you should mean it.** Estimate the work and add
a little. Too short is cheap to fix (`renew`); too long idles hardware someone
else is waiting for. Long soaks are fine — just renew as you go.

**`reason` is read by humans and other agents** when they are waiting for your
board. One short line: what you are testing.

Two boards that must talk to each other go in one claim, which is granted
together or not at all:

```json
claim {
  "slots": {"dut": ["soc=esp32s3"], "peer": ["family=esp32"]},
  "ttl_seconds": 900,
  "reason": "BLE mesh rejoin"
}
```

### 3. Use it with normal tools

The reply gives you an environment-variable name and a path per resource:

```
lease 7 — expires at 1788266909 (unix seconds)
  LAB_DUT_CONSOLE = /run/benchd/agent-3-s1/l7/dut/console
```

That is a real character device. Everything works as usual:

```sh
esptool --port "$LAB_DUT_CONSOLE" write-flash 0x0 firmware.bin
idf.py -p "$LAB_DUT_CONSOLE" monitor
minicom -D "$LAB_DUT_CONSOLE"
```

Use the path verbatim. Do not resolve it to whatever `/dev` node it points at, and
do not reuse a path from an earlier lease — the id in the path changes every time,
which is what stops a stale script writing to the wrong board.

### 4. Release as soon as you are done

```
release {"lease": 7}
```

Releasing early is free and someone may be waiting. A lease you forget expires on
its own, but only after the whole TTL you asked for.

## Reading the errors

The two failure modes need opposite responses, and the message says which:

**"this request cannot succeed as written"** — no bench will *ever* match. Do not
retry. Change the request; the message tells you how:

```
slot "dut": no bench exists matching {psram=octal soc=esp32c3}
  no bench has: soc=esp32c3
  drop soc=esp32c3 -> matches {psram=octal}
```

**"the hardware exists but is busy"** — wait and retry. The message says who has it
and for how long:

```
slot "dut": 2 bench(es) match {soc=esp32s3}, 0 free
  esp32s3-a: held by agent-7, expires in 240s
```

Wait roughly that long, then try again. Do not spin.

**"unknown tag ... (did you mean: ...)"** — a typo. Fix it and retry immediately.

## When a device stops working

If a device path gives **`ENOENT` or `EIO` mid-use, your lease ended.** The board
is fine. Do not power-cycle it, do not re-plug anything, do not investigate the
hardware. Claim again — and if the work needs longer, ask for a bigger `ttl_seconds`
this time.

You may also get a **revoking** notice before that happens: the grace window has
started and the device is about to go away. Either finish and `release`, or call
`renew` if you still need it.

## Rules

- Never touch `/dev/ttyUSB*` or `/dev/ttyACM*` directly.
- Never hardcode a bench name; describe what you need with tags.
- Claim the least capable thing that will work.
- Always release when done.
- A lost lease is routine, not an incident. Re-claim and carry on.
