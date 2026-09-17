//! The wire protocol: newline-delimited JSON over plain TCP (D5).
//!
//! All three binaries build from this one module, so there is no schema
//! language and no generated code — the types *are* the protocol. Multi-version
//! operation is an explicit non-goal, so nothing here carries a version number
//! or tolerates unknown fields.
//!
//! Two conversations, both **dialled by the executor** because the coordinator
//! may not be able to reach it (D5):
//!
//! ```text
//! host   → coordinator   HostMsg          coordinator → host    ToHost
//! client → coordinator   ClientMsg        coordinator → client  ToClient
//! ```
//!
//! Coordinator→executor messages carry a [`RequestId`]; the executor echoes it
//! in its reply. This is the correlation that RPC would have given us for free
//! in the other direction, and it is the whole cost of dialling inward.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::lease::{Epoch, LeaseId, SessionId};
use crate::limits::Secs;
use crate::model::Resource;
use crate::tags::Tag;

/// Correlates a coordinator request with the executor's reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// The public, agent-facing session handle. A UUID rather than the internal
/// dense [`SessionId`], so an agent cannot guess or collide with another
/// session by counting (D19).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionToken(pub String);

/// The default port. Nothing else in benchd listens.
pub const DEFAULT_PORT: u16 = 4711;

// ---------------------------------------------------------------------------
// Host ↔ coordinator
// ---------------------------------------------------------------------------

/// What a host declares about itself at registration.
///
/// The bench definition lives with the hardware (D9); only the *vocabulary* is
/// central, so these tags are validated by the coordinator and the registration
/// is refused if any is unknown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchSpec {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Markdown handed to the agent when this bench is granted: pinout, jumper
    /// positions, what is wired to what. Capped by the coordinator on
    /// registration; see [`crate::model::MAX_BENCH_DOCS`].
    #[serde(default)]
    pub docs: String,
    pub tags: Vec<Tag>,
    pub resources: BTreeMap<String, Resource>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum HostMsg {
    /// First message on the connection.
    Register {
        bench: BenchSpec,
    },
    Heartbeat,
    /// Reply to [`ToHost::Export`] / [`ToHost::Unexport`].
    Done {
        request: RequestId,
        result: Outcome,
    },
    /// A resource vanished from under us. The host releases and exits for a
    /// clean restart rather than trying to repair itself in place.
    DeviceLost {
        resource: String,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToHost {
    /// Registration accepted; the bench is now matchable.
    Registered,
    /// Registration refused — bad tags, duplicate bench id. The host should log
    /// and exit; retrying will not help.
    Rejected { reason: String },
    /// Make this bench's resources reachable by `session`.
    ///
    /// Maps each resource name to the rendezvous key the host must present when
    /// it dials out. Always populated, on every lease: a host hides its devices
    /// for its whole lifetime (D22), so there is no local inode to hand over
    /// even when the host and client share a machine.
    Export {
        request: RequestId,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
        #[serde(default)]
        channels: BTreeMap<String, ChannelKey>,
    },
    Unexport {
        request: RequestId,
        lease: LeaseId,
        epoch: Epoch,
    },
}

// ---------------------------------------------------------------------------
// Client ↔ coordinator
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ClientMsg {
    /// A new agent appeared on this machine. There is no authentication: this
    /// is a request for a token and it always succeeds (D19). `name` is a
    /// diagnostic label, not an authorisation input.
    OpenSession {
        request: RequestId,
        name: String,
    },
    CloseSession {
        request: RequestId,
        session: SessionToken,
    },

    Claim {
        request: RequestId,
        session: SessionToken,
        claim: ClaimSpec,
    },
    Renew {
        request: RequestId,
        session: SessionToken,
        lease: LeaseId,
        extra: Secs,
    },
    Release {
        request: RequestId,
        session: SessionToken,
        lease: LeaseId,
    },
    Status {
        request: RequestId,
        session: SessionToken,
    },
    TagList {
        request: RequestId,
    },
    /// Ask the local client daemon for every bench on every coordinator it is
    /// currently connected to. This never crosses a coordinator link: only the
    /// client knows that several independent authorities make up this machine's
    /// view, or which one was configured as `local`.
    Inspect {
        request: RequestId,
    },

    Heartbeat,
    /// Reply to [`ToClient::Materialize`] / [`ToClient::Unmaterialize`].
    Done {
        request: RequestId,
        result: Outcome,
    },

    /// Create this identity's lease directory and report where it is.
    ///
    /// Handled entirely by the client daemon; it never reaches the coordinator.
    /// The sandbox launcher calls this *before* starting an agent, because it
    /// must bind-mount that directory at launch — and the directory has to be
    /// created by root, not by the agent. An agent that can write its own lease
    /// directory can plant a symlink where root will later create the next
    /// lease, which turns a bind mount into an arbitrary-location one.
    PrepareOwner {
        request: RequestId,
        name: String,
    },
}

/// A claim as it crosses the wire. Mirrors [`crate::model::ClaimRequest`] but
/// with tags as strings, so a malformed tag produces a *diagnosable* error from
/// the coordinator's vocabulary rather than a parse failure at the edge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimSpec {
    /// slot name -> required tags, e.g. `{"dut": ["soc=esp32s3"]}`
    pub slots: BTreeMap<String, Vec<String>>,
    /// Mandatory. There is no default (D15).
    pub ttl: Secs,
    #[serde(default)]
    pub reason: String,
    #[serde(default = "default_true")]
    pub distinct: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToClient {
    /// Reply to `OpenSession`.
    ///
    /// Carries the internal id as well as the token: the client daemon needs it
    /// to tie a later `Materialize` back to an owner directory, and it is not
    /// secret — the token is what authorises, the id merely identifies.
    SessionOpened {
        request: RequestId,
        session: SessionToken,
        id: SessionId,
    },
    /// Reply to a request that succeeded but returns nothing.
    Ok { request: RequestId },
    /// Reply to `PrepareOwner`: the lease tree and private `/dev` to bind.
    OwnerReady {
        request: RequestId,
        path: String,
        device_path: String,
    },
    /// Reply to any request that failed. `retryable` is the machine-readable
    /// form of the unsatisfiable-versus-contended distinction (D14): an agent
    /// must never retry-spin on a request that can never succeed.
    Error {
        request: RequestId,
        error: String,
        retryable: bool,
    },

    /// A claim succeeded. Deliberately carries only the *assignment*, not
    /// paths: the coordinator does not know where the client will put the
    /// device nodes, and inventing a path here would be a second source of
    /// truth for the one thing D2 says must never be ambiguous. The client
    /// learns the paths when it executes the `Materialize` that follows.
    Granted {
        request: RequestId,
        lease: LeaseId,
        /// slot -> bench id
        slots: BTreeMap<String, String>,
        /// slot -> the bench's markdown documentation, where it has any.
        ///
        /// Delivered here rather than through discovery because this is the
        /// first moment the agent is entitled to know it: before the grant it
        /// has no bench, and a bench's wiring notes are exactly the sort of
        /// detail that would let it start addressing hardware by name (D3).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        docs: BTreeMap<String, String>,
        expires_at: Secs,
        /// Set when the granted TTL is shorter than the one requested.
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Renewed {
        request: RequestId,
        expires_at: Secs,
    },
    Status {
        request: RequestId,
        leases: Vec<LeaseStatus>,
    },
    Tags {
        request: RequestId,
        tags: Vec<TagInfo>,
    },
    /// Reply to [`ClientMsg::Inspect`], grouped by authority so equal lease and
    /// bench ids from different coordinators are never accidentally conflated.
    Inventory {
        request: RequestId,
        coordinators: Vec<CoordinatorInventory>,
    },

    /// Make a granted lease's devices appear for `session`.
    Materialize {
        request: RequestId,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
        /// slot -> resources to expose
        slots: BTreeMap<String, BTreeMap<String, ResourceHandle>>,
    },
    Unmaterialize {
        request: RequestId,
        lease: LeaseId,
        epoch: Epoch,
        session: SessionId,
    },

    /// Unsolicited: the grace window has started. Park the board or renew.
    Revoking {
        lease: LeaseId,
        reason: String,
        teardown_at: Secs,
    },
    /// Unsolicited: the lease is gone.
    Ended { lease: LeaseId, reason: String },
    /// Unsolicited: the lease was withdrawn before it ever worked. The holder
    /// is waiting for device paths that will now never arrive, so this is what
    /// stops it waiting.
    Failed { lease: LeaseId, detail: String },
}

/// Where a resource actually is, from the client's point of view.
///
/// One variant, deliberately. A host keeps its devices bound to the USB/IP
/// stub for its whole lifetime so that an unleased board has no tty for anyone
/// on that machine to open, which leaves no local inode to hand over even when
/// the host and client are the same box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum ResourceHandle {
    /// Dial the coordinator with this key, complete the USB/IP handshake, and
    /// hand the socket to the kernel (D5).
    ///
    /// Two resources of one bench may carry the *same* `channel`, which is how
    /// a device that produces several nodes is described: the import happens
    /// once and each resource then takes a different `node` from it.
    UsbIp {
        channel: ChannelKey,
        busid: String,
        #[serde(default)]
        node: WantedNode,
    },
}

/// Which device node a resource resolves to once its device is imported.
///
/// The client cannot work this out for itself. After a USB/IP import it has a
/// vhci port and a subtree of sysfs, and a device with two serial ports offers
/// two equally plausible ttys — so the host, which could still see the hardware
/// when it resolved the bench, says which one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WantedNode {
    /// A serial port. `interface` disambiguates a device with more than one;
    /// `None` means take whichever is found, which is correct for the single
    /// port case and is what older hosts send.
    Tty {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<u8>,
    },
    /// `/dev/sd*`.
    Block,
    /// `/dev/sg*`.
    Scsi,
}

/// A serial port with no interface preference: what every bench meant before
/// there was anything else to mean, and so what a handle without a `node`
/// deserialises to.
impl Default for WantedNode {
    fn default() -> Self {
        WantedNode::Tty { interface: None }
    }
}

/// Rendezvous key for one relayed data connection. Both ends dial out and
/// present it; the coordinator splices the two sockets that match.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChannelKey(pub String);

impl ChannelKey {
    /// A fresh, unguessable rendezvous key.
    ///
    /// **Deliberately random, not derived.** An earlier version built the key
    /// from `(lease, epoch, bench, resource)` so both ends could compute it
    /// independently — convenient, but every one of those values is either
    /// small, sequential, or discoverable from `lease_status`, so a third party
    /// on the LAN could guess a live key, win the rendezvous, and be spliced to
    /// someone else's device in place of the real one. The relay cannot tell
    /// the difference: presenting the key *is* the claim to the channel.
    ///
    /// The coordinator generates the key once and sends it to both sides, which
    /// costs one field in `ToHost::Export` and buys 122 bits of entropy.
    pub fn generate() -> Self {
        ChannelKey(uuid::Uuid::new_v4().to_string())
    }
}

/// First line on a data connection, before it becomes an opaque byte stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelHello {
    pub channel: ChannelKey,
    pub side: ChannelSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSide {
    Host,
    Client,
}

// ---------------------------------------------------------------------------
// Operator ↔ coordinator
//
// A separate surface from the agent one, not a privileged mode of it (D17).
// This is where naming a bench and taking it back live; agents can do neither.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum OperatorMsg {
    /// Everything: benches, who holds what, and why.
    Inspect,
    /// Take a bench back. `immediate` skips the grace window.
    ForceRelease { bench: String, immediate: bool },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToOperator {
    State {
        benches: Vec<BenchView>,
        leases: Vec<LeaseView>,
    },
    Released {
        count: usize,
    },
    Error {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchView {
    pub id: String,
    pub description: String,
    /// Whether the bench carries grant-time documentation, not the text itself:
    /// an operator auditing which benches still have no pinout notes needs the
    /// flag, and shipping every bench's markdown into every `inspect` would
    /// bury the thing they came to read.
    #[serde(default)]
    pub has_docs: bool,
    /// Rendered `key=value`, already sorted.
    pub tags: Vec<String>,
    pub resources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseView {
    pub id: u64,
    /// The holder's declared name. Diagnostic only (D19).
    pub owner: String,
    pub slots: BTreeMap<String, String>,
    pub expires_at: Secs,
    pub state: String,
    pub reason: String,
}

/// One coordinator's contribution to the local client's operator view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorInventory {
    /// The name from `benchd client --coordinator name=address`.
    pub name: String,
    /// True only for the coordinator explicitly named `local`.
    pub local: bool,
    pub benches: Vec<BenchView>,
    pub leases: Vec<LeaseView>,
}

// ---------------------------------------------------------------------------
// Shared payloads
// ---------------------------------------------------------------------------

/// An executor's report on an instruction it was given.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// The instruction was for a superseded lease and was ignored (D7). Not an
    /// error: it is the fencing working as designed.
    Stale {
        seen: Epoch,
    },
    Failed {
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaseStatus {
    pub lease: LeaseId,
    pub slots: BTreeMap<String, String>,
    pub expires_at: Secs,
    /// Seconds left. Sent explicitly because agents plan badly against
    /// absolute deadlines and well against countdowns.
    pub remaining: Secs,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TagInfo {
    pub tag: String,
    pub description: String,
    /// How many benches carry this tag, and how many are free right now.
    /// Availability lets an agent relax a request *before* claiming.
    pub benches: usize,
    pub free: usize,
}

/// The environment variable an agent should read for a materialised resource:
/// `dut` + `console` becomes `LAB_DUT_CONSOLE`.
///
/// Agents are reliably good at using `$LAB_DUT_CONSOLE` and reliably bad at
/// remembering which of two identical-looking device nodes was theirs.
pub fn env_var(slot: &str, resource: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("LAB_{}_{}", clean(slot), clean(resource))
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Serialise one message as a protocol line (no trailing newline).
pub fn encode<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(msg)
}

/// Parse one protocol line.
pub fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        decode(&encode(&value).unwrap()).unwrap()
    }

    #[test]
    fn messages_survive_a_round_trip() {
        let msg = ClientMsg::Claim {
            request: RequestId(7),
            session: SessionToken("2f8a".into()),
            claim: ClaimSpec {
                slots: [("dut".to_string(), vec!["soc=esp32s3".to_string()])]
                    .into_iter()
                    .collect(),
                ttl: 900,
                reason: "wifi reconnect regression".into(),
                distinct: true,
            },
        };
        assert_eq!(roundtrip(msg.clone()), msg);
    }

    #[test]
    fn the_wire_is_readable_because_socat_is_the_debugger() {
        // D5 keeps the protocol human-readable on purpose; this pins the shape
        // so a refactor cannot quietly turn it into something opaque.
        let line = encode(&ClientMsg::Renew {
            request: RequestId(3),
            session: SessionToken("2f8a".into()),
            lease: benchd_lease_id(4),
            extra: 300,
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"msg":"renew","request":3,"session":"2f8a","lease":4,"extra":300}"#
        );
    }

    #[test]
    fn env_vars_are_shouty_and_safe() {
        assert_eq!(env_var("dut", "console"), "LAB_DUT_CONSOLE");
        assert_eq!(env_var("node-a", "usb.0"), "LAB_NODE_A_USB_0");
    }

    #[test]
    fn a_resource_handle_says_how_to_reach_it() {
        let remote = encode(&ResourceHandle::UsbIp {
            channel: ChannelKey("k1".into()),
            busid: "1-2".into(),
            node: WantedNode::Tty { interface: None },
        })
        .unwrap();
        assert_eq!(
            remote,
            r#"{"via":"usb_ip","channel":"k1","busid":"1-2","node":{"kind":"tty"}}"#
        );
    }

    #[test]
    fn a_handle_from_a_host_that_predates_node_kinds_is_a_serial_port() {
        // Hosts and coordinators are upgraded separately, and the old meaning
        // of a handle was always "the tty on the other end of this channel".
        let old = r#"{"via":"usb_ip","channel":"k1","busid":"1-2"}"#;
        let handle: ResourceHandle = serde_json::from_str(old).unwrap();
        let ResourceHandle::UsbIp { node, .. } = handle;
        assert_eq!(node, WantedNode::Tty { interface: None });
    }

    #[test]
    fn two_resources_on_one_device_differ_only_in_the_node() {
        // The USB-SD-Mux case: one import, one channel, two nodes.
        let switch = ResourceHandle::UsbIp {
            channel: ChannelKey("k1".into()),
            busid: "3-1.1".into(),
            node: WantedNode::Scsi,
        };
        let card = ResourceHandle::UsbIp {
            channel: ChannelKey("k1".into()),
            busid: "3-1.1".into(),
            node: WantedNode::Block,
        };
        assert_ne!(switch, card);
        assert_eq!(roundtrip(switch.clone()), switch);
        assert_eq!(roundtrip(card.clone()), card);
    }

    #[test]
    fn a_stale_instruction_is_an_outcome_not_an_error() {
        // Fencing working as designed must not look like a failure (D7).
        let o = Outcome::Stale { seen: Epoch(9) };
        assert_eq!(encode(&o).unwrap(), r#"{"outcome":"stale","seen":9}"#);
        assert_eq!(roundtrip(o.clone()), o);
    }

    fn benchd_lease_id(n: u64) -> LeaseId {
        LeaseId(n)
    }
}
