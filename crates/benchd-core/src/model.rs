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
    /// A USB serial device, identified by its stable `/dev/serial/by-id` path.
    ///
    /// We key on by-id rather than `ttyUSB0` because kernel indices renumber on
    /// replug, and an agent handed the wrong board mid-session is the exact
    /// failure this system exists to prevent.
    Serial { by_id: PathBuf },
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
            Resource::Serial { by_id, .. } => std::fs::canonicalize(by_id).ok(),
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
    by_id: Option<PathBuf>,
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
                        by_id: res.by_id.ok_or_else(|| InventoryError::Bench {
                            bench: id.clone(),
                            reason: format!("serial resource {res_name:?} needs 'by_id'"),
                        })?,
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
