# Bench groups

Implemented from `v0.1.0-beta.3` for `0.2.0-beta1` (protocol 2).

## Contract

A bench remains the indivisible lease unit. Optional, nonoverlapping groups
represent independently usable benches in one physical setup across hosts under
one coordinator. Claims choose `none` (default), `same`, or `exclusive`.
`same` reserves selected benches; `exclusive` blocks all members until lease
teardown completes or reaches its existing deadline. New members inherit that
reservation. Only selected benches are exported and counted toward session limits.
No merge or upgrade of existing leases is supported.

## Implementation

- Carry membership through config, host registration and inventory.
- Enforce a common group inside bounded best-fit assignment search. Diagnose
  structural feasibility separately from contention.
- Expand exclusive leases into busy inventory members and retain group metadata
  in teardown effects. Preserve draining membership even when a host disconnects.
- Guard membership changes before registration can displace a live host. Apply
  the same guard to standby-host restoration.
- Carry selected group and mode through grant forwarding, CLI/MCP and status.
- Bump protocol to 2 so older peers cannot silently ignore grouping constraints.
- Exercise unit/property tests and real coordinator sockets; independent review
  covers lifecycle, registration and matcher behavior.

## Separate increment

Wi-Fi/Bluetooth adapter resource kinds and USB/IP network-device materialization
are not implemented here. The four-role claim syntax is supported once those
capabilities/resources exist in a lab. Groups do not imply RF isolation.

## Validation

- `cargo test --workspace --all-features`: 227 passed.
- `cargo test -p benchd --test daemons -- --ignored --test-threads=1`: 29 passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all --check` and `git diff --check`: passed.
- Independent adversarial review found and verified fixes for teardown
  misclassifying impossible cross-group requests as retryable, and operator
  force-release failing to reclaim an unselected exclusive-group member.

These checks use throwaway unprivileged coordinator processes. Actual radio
adapter materialization remains outside this increment.

The follow-up adversarial review found that rejecting the newest standby could
strand an older compatible host. Withdrawal now tries remaining standbys and
removes rejected entries before hangup, preserving the restored host during
later socket cleanup. The new daemon regression failed before this fix and
passes with two consecutive incompatible standbys; independent review also
verified reservation retention and the original reproduction.
