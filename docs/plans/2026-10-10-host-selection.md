# Host selection and atomic claims implementation plan

**Goal:** Label benches by host, filter discovery and claims by that label, and verify all-or-nothing multi-bench reservations.

**Architecture:** Reuse `host=<name>` tags throughout registration, discovery, CLI and MCP claims. A host process supplies its lowercase kernel hostname unless the bench config declares a host tag. Keep the existing named-slot claim transaction within one coordinator, which may manage multiple physical hosts. Host identity must not affect capability scarcity scoring.

**Tech stack:** Rust, clap, serde, existing JSON-lines daemon protocol.

1. Add failing tests in `crates/benchd-core/tests/hosts.rs` for built-in host vocabulary support, neutral matching cost, and atomic claims when a requested host is unavailable or busy.
2. Add host registration tests in `crates/benchd-host/src/lib.rs` for automatic and explicit host tags, then implement tag generation without changing the wire format.
3. Add daemon integration coverage in `crates/benchd/tests/daemons.rs` for host-tag registration, CLI filters and multi-host slot allocation. Implement positional tag filters and `--host` in `crates/benchd-coordinator/src/operator.rs`.
4. Document config, CLI and MCP examples in `README.md`, `examples/coordinator.toml`, and `plugins/benchd/skills/benchd/SKILL.md`.
5. Run focused tests first, then `cargo test --workspace`, daemon integration tests serially, formatting checks and clippy. Review the final diff, preserving the existing `hook.rs` edit.

Cross-coordinator distributed transactions are outside this implementation. Existing hosts without a host tag remain usable by capability; upgrading/restarting hosts supplies the new default tag. Upgrade coordinators before hosts so they accept the new tag.

## Validation

Implemented all five steps. The user confirmed one coordinator with multiple hosts.
The existing atomic slot allocator required no changes; new coverage checks host
selection, busy/missing slots, distinctness, one shared lease and release.

- After removing the sandbox restriction and including the protocol-handshake
  changes, `cargo test --workspace --no-fail-fast -- --test-threads=1` passes:
  207 passed, 0 failed, 22 ignored.
- `cargo test -p benchd --test daemons -- --ignored --test-threads=1` passes
  all 22 daemon tests, including host filters, atomic multi-host claims and
  whole-lease teardown after setup failure.
- `cargo clippy --workspace --all-targets -- -D warnings`, formatting, and diff
  whitespace checks pass.
- Independent review found no concrete defects.
- An existing concurrent-queue test failed during the first parallel workspace
  run, then passed in the serial run. Its producer unblocks the consumer before
  reporting its own event, making the asserted event order scheduler-dependent;
  this unrelated code was left unchanged.
