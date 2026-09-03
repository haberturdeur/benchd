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
    Io { path: PathBuf, source: std::io::Error },
    #[error("failed to parse {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
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
    Serial { path: PathBuf, serial: Option<String> },
    /// A whole USB device, for USB/IP export to a remote client. Carries the
    /// bus id (`1-2.3`) that `usbip bind` needs. Not handled by the local
    /// bind-mount backend.
    Usb { busid: String },
}

// A resource has no `name` field: it is always stored in a map keyed by its
// name, and carrying the name in both places invites them to disagree.

impl Resource {
    pub fn kind(&self) -> &'static str {
        match self {
            Resource::Serial { .. } => "serial",
            Resource::Usb { .. } => "usb",
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
    pub id: String,
    /// Fully expanded through the implication graph, plus an injected
    /// `name=<id>` tag so human/debug selection rides the same matching path as
    /// everything else.
    pub tags: TagSet,
    pub resources: BTreeMap<String, Resource>,
    pub description: String,
    /// Set false to keep a bench in the inventory but out of the matcher (dead
    /// board, cable being reseated) without deleting its definition.
    pub enabled: bool,
}

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
        Ok(Requirement { tags: parse_tags(items)? })
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
/// A host declares this path and the client daemon — running as root —
/// bind-mounts it and hands it to the agent's uid. Anyone who can reach the
/// coordinator can register a bench (§9 accepts that), so without this check a
/// registration string reaches `mount(2)` and `chown(2)` unvalidated: register a
/// bench whose "device" is `/etc/shadow`, claim it, and the file is mounted into
/// your sandbox owned by you.
///
/// Devices live under `/dev`. Nothing else is a device, so nothing else is
/// accepted.
pub fn valid_device_path(path: &std::path::Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("{} contains '..'", path.display()));
    }
    if !path.starts_with("/dev/") {
        return Err(format!("{} is not under /dev/", path.display()));
    }
    Ok(())
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
    pub slots: BTreeMap<String, Requirement>,
    pub distinct: Distinct,
    /// Mandatory. There is no default: making the agent name a duration forces
    /// it to scope the work, and gives us requested-vs-used telemetry.
    pub ttl_seconds: u64,
    pub reason: String,
}

impl ClaimRequest {
    /// Reject anything that could escape a directory once materialised.
    ///
    /// Slot names are chosen by the agent and become path components inside a
    /// root daemon, so this is a privilege boundary, not tidiness.
    pub fn validate(&self) -> Result<(), String> {
        if self.slots.is_empty() {
            return Err("a claim must request at least one slot".into());
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

    /// How many enabled benches carry each tag. Drives scarcity scoring.
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
        let text = std::fs::read_to_string(path)
            .map_err(|source| InventoryError::Io { path: path.to_path_buf(), source })?;
        let raw: RawInventory = toml::from_str(&text)
            .map_err(|source| InventoryError::Parse { path: path.to_path_buf(), source })?;
        raw.build()
    }

    pub fn from_toml_str(text: &str) -> Result<Self, InventoryError> {
        let raw: RawInventory =
            toml::from_str(text).map_err(|source| InventoryError::Parse {
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
    #[serde(default)]
    description: String,
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

        for (key, body) in &self.tags {
            if let Some(weight) = body.weight {
                key_weights.insert(key.clone(), weight);
            }
            if !body.description.is_empty() {
                key_descriptions.insert(key.clone(), body.description.clone());
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
        let vocabulary = Vocabulary::new(defs, open_keys, key_weights, key_descriptions)?;

        let mut benches = BTreeMap::new();
        for (id, body) in self.benches {
            let declared = parse_tags(&body.tags)?;
            vocabulary.check(&declared)?;

            let mut tags = vocabulary.expand(&declared);
            // Inject the bench id as a tag so human/debug selection by name
            // rides the same matching path as everything else. Round-tripping
            // through the parser means a bench id that could never be written
            // as `name=<id>` fails at load time rather than producing a bench
            // nobody can select.
            let name_tag = Tag::parse(&format!("name={id}")).map_err(|e| InventoryError::Bench {
                bench: id.clone(),
                reason: format!("bench id is not usable as a tag value: {e}"),
            })?;
            tags.insert(name_tag);

            let mut resources = BTreeMap::new();
            for (res_name, res) in body.resources {
                let resource = match res.kind.as_str() {
                    "serial" => Resource::Serial {
                        path: res.by_path.or(res.by_id).ok_or_else(|| {
                            InventoryError::Bench {
                                bench: id.clone(),
                                reason: format!(
                                    "serial resource {res_name:?} needs 'by_path' \
                                     (preferred) or 'by_id'"
                                ),
                            }
                        })?,
                        serial: res.serial,
                    },
                    "usb" => Resource::Usb {
                        busid: res.busid.ok_or_else(|| InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!("usb resource {res_name:?} needs 'busid'"),
                        })?,
                    },
                    other => {
                        return Err(InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!("resource {res_name:?} has unknown kind {other:?}"),
                        })
                    }
                };
                resources.insert(res_name, resource);
            }

            benches.insert(
                id.clone(),
                Bench {
                    id,
                    tags,
                    resources,
                    description: body.description,
                    enabled: body.enabled,
                },
            );
        }

        Ok(Inventory { benches, vocabulary })
    }
}
