//! Inventory model: resources, benches, and claim requirements.
//!
//! A **bench** is the unit of exclusion: a named set of physical resources that
//! must be held together (the board, its USB-JTAG interface, the relay that
//! power-cycles it). Agents never name a bench; they describe one with tags and
//! the matcher finds it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tags::{parse_tags, Tag, TagDef, TagError, TagSet, Vocabulary};

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("{0}")]
    Tag(#[from] TagError),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("bench {bench}: {reason}")]
    Bench { bench: String, reason: String },
}

/// Something that gets materialised into a lease directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resource {
    /// A USB serial device, named by a stable path under `/dev/serial/`.
    ///
    /// Never `ttyUSB0`: kernel indices renumber on replug, and an agent handed
    /// the wrong board mid-session is the exact failure this system exists to
    /// prevent.
    ///
    /// Two stable names exist and they mean different things:
    ///
    /// * `by-path` identifies a **physical position** — this port on this hub.
    ///   Whatever is plugged in there is the bench.
    /// * `by-id` identifies a **specific chip**, by vendor, product and serial.
    ///
    /// `by-path` is the better default for a lab, because a bench *is* a
    /// position: swap a dead board for a fresh one and nothing needs editing.
    /// `by-id` breaks loudly on a swap, which is right only when a particular
    /// board matters more than the slot it sits in. Either is accepted.
    ///
    /// `serial` is the USB serial number of the chip that is *expected* in that
    /// position — for an ESP32 that is its MAC. Position and identity answer
    /// different questions and a bench wants both: the path says which slot,
    /// the serial says whether the thing in it is still the thing the tags
    /// describe. Declare it and a silent board swap is refused at registration
    /// instead of handing an agent an ESP32-C3 that every tag calls an S3.
    /// Leave it out and whatever is in the slot is accepted.
    ///
    /// `interface` is the USB interface number the tty hangs off, filled in by
    /// the host when it resolves `path`. It exists because a device can expose
    /// more than one serial port: on an FT2232H the two channels are interfaces
    /// 0 and 1, and on a WROVER-KIT one of them is the JTAG channel and the
    /// other is the console. Without it, locating the tty after a USB/IP import
    /// means taking whichever one the kernel lists first, which is a coin toss.
    /// Nobody writes this by hand — the `by_path` already names the interface.
    Serial {
        path: PathBuf,
        serial: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<u8>,
    },
    /// A whole USB device, named directly by the bus id (`1-2.3`) that `usbip
    /// bind` needs, rather than resolved from a tty like [`Resource::Serial`].
    ///
    /// `node` says which of the device's device nodes the agent wants. One USB
    /// device can produce several at once — a USB-SD-Mux is switched through
    /// its SCSI generic node and written through its block node — so two
    /// resources may name the same `busid` and differ only here. USB/IP still
    /// forwards the device once; the fan-out happens after the import.
    Usb { busid: String, node: UsbNode },
}

/// Which of a USB device's nodes a resource wants.
///
/// Deliberately not "the whole device": an agent needs a path it can hand to
/// `dd` or `usbsdmux`, and a resource that resolved to a directory of nodes
/// would push the job of picking one onto the agent, which cannot see the
/// device to pick correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbNode {
    /// `/dev/sd*` — the storage itself.
    Block,
    /// `/dev/sg*` — the SCSI generic node, which is how `usbsdmux` sends the
    /// vendor command that switches the card between the DUT and the host.
    Scsi,
}

impl UsbNode {
    pub fn as_str(self) -> &'static str {
        match self {
            UsbNode::Block => "block",
            UsbNode::Scsi => "scsi",
        }
    }
}

// A resource has no `name` field: it is always stored in a map keyed by its
// name, and carrying the name in both places invites them to disagree.

impl Resource {
    /// The `kind =` a bench config would write for this resource. Not the same
    /// spelling as the wire tag, which says `usb` and puts the node beside it.
    pub fn config_kind(&self) -> &'static str {
        match self {
            Resource::Serial { .. } => "serial",
            Resource::Usb { node, .. } => node.as_str(),
        }
    }

    /// What distinguishes the *device* behind this resource, for grouping.
    ///
    /// Two resources with the same key are two views of one USB device and must
    /// share a single USB/IP channel, because USB/IP forwards whole devices. A
    /// serial resource falls back to its own name: the coordinator never learns
    /// the busid behind a tty, and the host refuses a bench where two serial
    /// resources resolve to one device rather than let the two disagree here.
    pub fn device_key<'a>(&'a self, name: &'a str) -> &'a str {
        match self {
            Resource::Usb { busid, .. } => busid,
            Resource::Serial { .. } => name,
        }
    }

    /// Resolve a serial resource to its current `/dev/ttyX` node.
    pub fn resolve(&self) -> Option<PathBuf> {
        match self {
            Resource::Serial { path, .. } => std::fs::canonicalize(path).ok(),
            Resource::Usb { .. } => None,
        }
    }
}

/// A named, atomically-claimable set of resources.
#[derive(Clone, Debug)]
pub struct Bench {
    /// Optional physical setup of independently claimable benches.
    pub group: Option<String>,
    pub id: String,
    /// Fully expanded through the implication graph, plus an injected
    /// `name=<id>` tag so human/debug selection rides the same matching path as
    /// everything else.
    pub tags: TagSet,
    pub resources: BTreeMap<String, Resource>,
    pub description: String,
    /// Markdown handed to the agent at grant time: pinout, jumpers, what is
    /// wired to what. Empty for most benches.
    pub docs: String,
    /// Set false to keep a bench in the inventory but out of the matcher (dead
    /// board, cable being reseated) without deleting its definition.
    ///
    /// Only an inventory loaded from a file can set it today. There is no
    /// `enabled` on [`crate::wire::BenchSpec`], so a host cannot declare one
    /// and the coordinator — whose inventory arrives entirely by registration —
    /// always builds benches enabled.
    pub enabled: bool,
}

/// Ceiling on a bench's documentation, in bytes.
///
/// Generous for the pinout and jumper notes this is for, small enough that a
/// host cannot push a datasheet into every agent's context window. Enforced
/// both when loading a file and when a host registers, because the host is not
/// necessarily running the same build as the coordinator.
pub const MAX_BENCH_DOCS: usize = 8 * 1024;

impl Bench {
    /// Resource names in stable order. A bench may hold several boards (D11);
    /// claiming it yields every one of these.
    pub fn resource_names(&self) -> Vec<&str> {
        self.resources.keys().map(String::as_str).collect()
    }
}

/// The tag key that carries a bench's own id.
///
/// Injected per bench so that human and operator selection rides the same
/// matching path as everything else — but agents must never use it (D17), or
/// one will hardcode a bench into a test script and reintroduce exactly the
/// contention this system removes.
pub const NAME_KEY: &str = "name";

/// Validate the tags a bench *declares*, as opposed to the tags a claim asks
/// for.
///
/// [`Vocabulary::check`] plus one rule: nobody declares their own [`NAME_KEY`].
/// It is an open key because bench ids cannot be enumerated centrally, which
/// makes it the one tag a declaration cannot be checked against — and anyone
/// who can reach the coordinator may register a bench (§9), so a declared
/// `name=esp32s3-a` simply *is* esp32s3-a everywhere matching happens. The real
/// one is injected from the bench id, by whoever knows what that id is.
///
/// Both places that build a [`Bench`] from declared tags must call this: the
/// inventory loader below, and the coordinator's host registration.
pub fn check_declared_tags<'a, I>(vocabulary: &Vocabulary, tags: I) -> Result<(), TagError>
where
    I: IntoIterator<Item = &'a Tag>,
{
    for tag in tags {
        if tag.key == NAME_KEY {
            return Err(TagError::Vocabulary(format!(
                "{tag}: a bench does not declare {NAME_KEY}=, it is assigned from the \
                 bench id"
            )));
        }
        vocabulary.check([tag])?;
    }
    Ok(())
}

/// Tags a single slot must satisfy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub tags: TagSet,
}

impl Requirement {
    pub fn new(tags: TagSet) -> Self {
        Requirement { tags }
    }

    pub fn parse<I, S>(items: I) -> Result<Self, TagError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Ok(Requirement {
            tags: parse_tags(items)?,
        })
    }

    /// The entire matching semantic, in one line: a bench matches when its tags
    /// are a superset of what was asked for.
    pub fn matches(&self, bench: &Bench) -> bool {
        self.tags.is_subset(&bench.tags)
    }
}

/// Names that become directory components on a privileged daemon.
///
/// Slot names come from an agent's claim and resource names from a host's
/// config, and both end up in `<root>/<owner>/<lease>/<slot>/<resource>` inside
/// a process running as root. `Path::join` does not normalise `..`, and an
/// absolute component *replaces* everything before it — so an unvalidated name
/// is an arbitrary-location `mount --bind` with root privileges.
///
/// This is the first of two defences; the materialiser also refuses to mount
/// outside its root. Neither is sufficient alone, because this one runs in the
/// coordinator and the materialiser must not trust the coordinator either.
pub fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Where a bench's serial resources are allowed to live.
///
/// Written for a specific escalation: the declared path used to be handed
/// straight to the client daemon, which bind-mounted it as root and chowned it
/// to the agent — so registering a bench whose "device" was `/etc/shadow` put
/// that file in your sandbox, owned by you. Anyone who can reach the
/// coordinator can register a bench (§9 accepts that), and nothing checked.
///
/// That path is gone: a client only ever mounts a node the kernel produced for
/// an imported device, never one a host named. The check stays anyway. It is
/// two comparisons on an open registration surface, the declared path is still
/// canonicalised by a root host process, and a bench that names something which
/// is not a device could never work regardless.
///
/// Devices live under `/dev`. Nothing else is a device, so nothing else is
/// accepted.
pub fn valid_device_path(path: &std::path::Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("{} contains '..'", path.display()));
    }
    if !path.starts_with("/dev/") {
        return Err(format!("{} is not under /dev/", path.display()));
    }
    Ok(())
}

/// Relationship required between independently claimable benches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    /// Independent selection, including ungrouped benches.
    #[default]
    None,
    /// One nonempty group, reserving selected benches only.
    Same,
    /// One nonempty group, reserving every member.
    Exclusive,
}

impl Grouping {
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Same => "same",
            Self::Exclusive => "exclusive",
        }
    }
}

impl std::str::FromStr for Grouping {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "same" => Ok(Self::Same),
            "exclusive" => Ok(Self::Exclusive),
            _ => Err("grouping must be none, same, or exclusive".into()),
        }
    }
}

/// Which slots must land on *different* benches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Distinct {
    All,
    None,
    Only(BTreeSet<String>),
}

impl Distinct {
    pub fn applies_to(&self, slot: &str) -> bool {
        match self {
            Distinct::All => true,
            Distinct::None => false,
            Distinct::Only(set) => set.contains(slot),
        }
    }
}

/// A request for one or more benches, granted atomically or not at all.
///
/// Partial allocation is never returned: two agents each holding half of what
/// they need is a deadlock, so the matcher either satisfies every slot or
/// reports why it cannot.
#[derive(Clone, Debug)]
pub struct ClaimRequest {
    pub grouping: Grouping,
    pub slots: BTreeMap<String, Requirement>,
    pub distinct: Distinct,
    /// Mandatory. There is no default: making the agent name a duration forces
    /// it to scope the work, and gives us requested-vs-used telemetry.
    pub ttl_seconds: u64,
    pub reason: String,
}

/// Ceiling on the number of slots in one claim.
///
/// Not a policy limit — `max_benches` is that, and it counts benches. This
/// bounds the *assignment search*, which is exponential in slots and runs
/// while the coordinator holds its single state lock. Slot-counted admission
/// used to impose such a ceiling by accident; counting benches, which is what
/// `max_benches` means, took it away. No real claim names more slots than a
/// bench has boards on it.
pub const MAX_SLOTS: usize = 16;

impl ClaimRequest {
    /// Reject anything that could escape a directory once materialised.
    ///
    /// Slot names are chosen by the agent and become path components inside a
    /// root daemon, so this is a privilege boundary, not tidiness.
    pub fn validate(&self) -> Result<(), String> {
        if self.slots.is_empty() {
            return Err("a claim must request at least one slot".into());
        }
        if self.slots.len() > MAX_SLOTS {
            return Err(format!(
                "a claim may name at most {MAX_SLOTS} slots; this one names {}",
                self.slots.len()
            ));
        }
        for slot in self.slots.keys() {
            if !valid_component(slot) {
                return Err(format!(
                    "invalid slot name {slot:?}: use a short plain name such as \"dut\" \
                     (letters, digits, dash, underscore, dot)"
                ));
            }
        }
        Ok(())
    }
}

/// The set of known benches plus the vocabulary describing them.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub benches: BTreeMap<String, Bench>,
    pub vocabulary: Vocabulary,
}

impl Inventory {
    pub fn enabled_benches(&self) -> Vec<&Bench> {
        self.benches.values().filter(|b| b.enabled).collect()
    }

    /// How many enabled benches carry each tag, free or not — what `benchd
    /// tags` reports as the size of the lab. Scarcity scoring needs the free
    /// count instead and derives its own; see [`crate::matcher::allocate`].
    pub fn tag_counts(&self) -> BTreeMap<Tag, usize> {
        let mut counts = BTreeMap::new();
        for bench in self.benches.values().filter(|b| b.enabled) {
            for tag in &bench.tags {
                *counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, InventoryError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| InventoryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let raw: RawInventory = toml::from_str(&text).map_err(|source| InventoryError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        raw.build()
    }

    pub fn from_toml_str(text: &str) -> Result<Self, InventoryError> {
        let raw: RawInventory = toml::from_str(text).map_err(|source| InventoryError::Parse {
            path: PathBuf::from("<inline>"),
            source,
        })?;
        raw.build()
    }
}

// ---------------------------------------------------------------------------
// Config schema. Kept separate from the domain types so the on-disk format can
// change without the matcher noticing.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawInventory {
    #[serde(default)]
    open_keys: Option<Vec<String>>,
    #[serde(default)]
    tags: BTreeMap<String, RawTagKey>,
    #[serde(default)]
    benches: BTreeMap<String, RawBench>,
}

#[derive(Debug, Deserialize)]
struct RawTagKey {
    #[serde(default)]
    description: String,
    /// Per-key best-fit weight; see `Vocabulary::weight`.
    weight: Option<f64>,
    /// Whether a bench may write `key=category[part]` on this key.
    #[serde(default)]
    qualified: bool,
    #[serde(default)]
    values: BTreeMap<String, RawTagValue>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTagValue {
    #[serde(default)]
    description: String,
    #[serde(default)]
    implies: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawBench {
    group: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    docs: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    resources: BTreeMap<String, RawResource>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawResource {
    #[serde(default = "default_kind")]
    kind: String,
    /// Preferred: identifies the physical port, so swapping the board in it
    /// needs no config change.
    by_path: Option<PathBuf>,
    /// Alternative: identifies one specific chip.
    by_id: Option<PathBuf>,
    /// The USB serial number expected in this position, if it matters.
    serial: Option<String>,
    /// Which USB interface the tty hangs off. Normally derived from the path
    /// rather than written by hand.
    interface: Option<u8>,
    busid: Option<String>,
}

fn default_kind() -> String {
    "serial".to_string()
}

impl RawInventory {
    fn build(self) -> Result<Inventory, InventoryError> {
        let mut defs = BTreeMap::new();
        let mut key_weights = BTreeMap::new();
        let mut key_descriptions = BTreeMap::new();
        let mut qualified_keys = BTreeSet::new();

        for (key, body) in &self.tags {
            if let Some(weight) = body.weight {
                key_weights.insert(key.clone(), weight);
            }
            if !body.description.is_empty() {
                key_descriptions.insert(key.clone(), body.description.clone());
            }
            if body.qualified {
                qualified_keys.insert(key.clone());
            }
            for (value, val_body) in &body.values {
                // Round-trip through the parser so config-declared tags obey
                // exactly the same rules as agent-supplied ones.
                let tag = Tag::parse(&format!("{key}={value}"))?;
                defs.insert(
                    tag,
                    TagDef {
                        description: val_body.description.clone(),
                        implies: parse_tags(&val_body.implies)?,
                    },
                );
            }
        }

        let open_keys: BTreeSet<String> = self
            .open_keys
            .unwrap_or_else(|| vec!["name".to_string()])
            .into_iter()
            .collect();
        let vocabulary = Vocabulary::new(
            defs,
            open_keys,
            key_weights,
            key_descriptions,
            qualified_keys,
        )?;

        let mut benches = BTreeMap::new();
        for (id, body) in self.benches {
            let declared = parse_tags(&body.tags)?;
            check_declared_tags(&vocabulary, &declared)?;

            let mut tags = vocabulary.expand(&declared);
            // Inject the bench id as a tag so human/debug selection by name
            // rides the same matching path as everything else. Round-tripping
            // through the parser means a bench id that could never be written
            // as `name=<id>` fails at load time rather than producing a bench
            // nobody can select.
            let name_tag =
                Tag::parse(&format!("name={id}")).map_err(|e| InventoryError::Bench {
                    bench: id.clone(),
                    reason: format!("bench id is not usable as a tag value: {e}"),
                })?;
            tags.insert(name_tag);

            if let Some(group) = &body.group {
                if !valid_component(group) {
                    return Err(InventoryError::Bench {
                        bench: id.clone(),
                        reason: "invalid group identifier".into(),
                    });
                }
            }
            let mut resources = BTreeMap::new();
            for (res_name, res) in body.resources {
                let resource = match res.kind.as_str() {
                    "serial" => Resource::Serial {
                        path: res
                            .by_path
                            .or(res.by_id)
                            .ok_or_else(|| InventoryError::Bench {
                                bench: id.clone(),
                                reason: format!(
                                    "serial resource {res_name:?} needs 'by_path' \
                                     (preferred) or 'by_id'"
                                ),
                            })?,
                        serial: res.serial,
                        interface: res.interface,
                    },
                    kind @ ("block" | "scsi") => Resource::Usb {
                        busid: res.busid.ok_or_else(|| InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!("{kind} resource {res_name:?} needs 'busid'"),
                        })?,
                        node: if kind == "block" {
                            UsbNode::Block
                        } else {
                            UsbNode::Scsi
                        },
                    },
                    // `usb` used to mean "the whole device" and had no way to
                    // say which node the agent wanted, so such a bench
                    // registered happily and then hung at materialisation
                    // waiting for a tty that would never appear. Failing here
                    // is the same error ten minutes earlier.
                    "usb" => {
                        return Err(InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!(
                                "resource {res_name:?}: kind 'usb' no longer says enough — \
                                 use 'block' for the storage node or 'scsi' for the \
                                 control node"
                            ),
                        })
                    }
                    other => {
                        return Err(InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!("resource {res_name:?} has unknown kind {other:?}"),
                        })
                    }
                };
                resources.insert(res_name, resource);
            }

            if body.docs.len() > MAX_BENCH_DOCS {
                return Err(InventoryError::Bench {
                    bench: id.clone(),
                    reason: format!(
                        "docs are {} bytes, over the {MAX_BENCH_DOCS} byte limit",
                        body.docs.len()
                    ),
                });
            }

            benches.insert(
                id.clone(),
                Bench {
                    group: body.group.clone(),
                    id,
                    tags,
                    resources,
                    description: body.description,
                    docs: body.docs,
                    enabled: body.enabled,
                },
            );
        }

        Ok(Inventory {
            benches,
            vocabulary,
        })
    }
}
