# Protocol compatibility handshake

**Goal:** Detect incompatible benchd deployments before registering hardware,
creating sessions or leases, or handing a data connection to USB/IP.

**Architecture:** Every benchd TCP and Unix connection begins with a bounded,
timed JSON hello containing an explicit protocol number, package version and
build identifier. Compatibility depends on equal protocol numbers; build
differences are diagnostic. Reject unversioned peers, with a message understood
by the older control protocol where possible. Read only through the hello's
newline so pipelined control or binary payload bytes survive.

**Implementation:**

1. Add pure version metadata and shared async handshake helpers in benchd-core,
   plus a build script recording Git revision/dirty state or BENCHD_BUILD_ID.
2. Test compatibility, mismatch, legacy peers, malformed/oversize messages,
   deadlines and preservation of pipelined bytes using in-memory duplex streams.
3. Require the handshake on coordinator and client-daemon listeners and every
   outgoing control/data connection, before exposing those connections as live.
4. Preserve useful mismatch errors through MCP reconnect handling. Route sandbox
   owner preparation through the binary so the shell does not duplicate protocol
   constants or handshake logic.
5. Update existing integration peers, add mismatch coverage, expose build details
   with --version, document the upgrade boundary, and run tests/lints/review.

Existing feature and hook edits in this workspace must be preserved. No deployment
or service restart is part of this change.

## Validation

All implementation steps are complete. The shared handshake has 11 passing
in-memory tests; MCP mismatch propagation has an additional passing regression
test. A message-kind regression exposed that Serde's tagged structs do not
validate the tag when deserializing; an explicit enum field now enforces `hello`.

After removing the sandbox restriction, verification passes completely:

- `cargo test --workspace --no-fail-fast -- --test-threads=1`: 207 passed,
  0 failed, 22 ignored. This includes the MCP reconnect socket test.
- `cargo test -p benchd --test daemons -- --ignored --test-threads=1`:
  all 22 daemon integration tests passed, including rejection of mismatched and
  unversioned peers and atomic claims with host filters.

No code fixes were needed after removing the restriction. Formatting, clippy
with warnings denied, shell syntax and diff whitespace checks also passed.
A read-only review found no actionable defects.

`benchd --version` was checked and includes package, protocol and build values.
No installed binaries or running services were changed.
