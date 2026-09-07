//! Tag vocabulary, validation, and implication closure.
//!
//! Tags are `key=value` pairs. A tag *set* is a [`BTreeSet<Tag>`], which means a
//! key may legitimately appear more than once (`sensor=bme280` and
//! `sensor=sht31` on the same bench). Matching is plain subset containment, so
//! multi-valued keys need no special handling.
//!
//! `BTreeSet` rather than `HashSet` is deliberate: iteration order is stable, so
//! rendered diagnostics and test assertions are deterministic.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// A `key=value` pair.
///
/// Ordered by (key, value) so that rendered tag lists are stable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag {
    pub key: String,
    pub value: String,
}

// Tags cross the wire and appear in config as `"soc=esp32s3"`, not as a struct
// with two fields. Serialising through the same parser the rest of the system
// uses means a malformed tag is rejected identically wherever it arrives.
impl Serialize for Tag {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Tag::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl Tag {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Tag {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Parse `"key=value"`.
    ///
    /// Keys are `[a-z][a-z0-9_]*`. Values are `[a-z0-9][a-z0-9_.-]*`.
    ///
    /// Dashes are accepted *here* but rejected in the vocabulary (see
    /// `Vocabulary::validate`): capability values must not admit the
    /// `esp32-s3` / `esp32s3` split that makes tag vocabularies rot, but
    /// identity values like `name=esp32s3-a` are opaque strings and need them.
    pub fn parse(text: &str) -> Result<Tag, TagError> {
        let Some((key, value)) = text.split_once('=') else {
            return Err(TagError::Malformed {
                text: text.to_string(),
                reason: "expected key=value".into(),
            });
        };
        let key = key.trim();
        let value = value.trim();

        if !valid_key(key) {
            return Err(TagError::Malformed {
                text: text.to_string(),
                reason: format!("invalid key {key:?} (expected [a-z][a-z0-9_]*)"),
            });
        }
        if !valid_value(value) {
            return Err(TagError::Malformed {
                text: text.to_string(),
                reason: format!("invalid value {value:?} (expected [a-z0-9][a-z0-9_.-]*)"),
            });
        }
        Ok(Tag::new(key, value))
    }

    /// The value with any qualifier stripped: `accel` for `accel[mpu6050]`.
    pub fn base(&self) -> &str {
        match self.value.split_once('[') {
            Some((base, _)) => base,
            None => &self.value,
        }
    }

    /// The free-form part identity inside the brackets, if there is one.
    ///
    /// A qualifier says *which* accelerometer a board carries without the
    /// vocabulary having to know every accelerometer that exists. The category
    /// is curated centrally because agents match on it; the part is asserted by
    /// whoever is looking at the board, who is the only one who reliably knows.
    pub fn qualifier(&self) -> Option<&str> {
        self.value.split_once('[')?.1.strip_suffix(']')
    }

    /// This tag with its qualifier removed.
    pub fn base_tag(&self) -> Tag {
        Tag::new(self.key.clone(), self.base())
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A value, optionally carrying a `[qualifier]`.
///
/// Both halves obey the same character rules rather than the brackets loosening
/// them: `accel[mpu6050]` is two ordinary values with a separator, not a new
/// kind of string.
fn valid_value(value: &str) -> bool {
    match value.split_once('[') {
        None => valid_plain(value),
        Some((base, rest)) => match rest.strip_suffix(']') {
            Some(qualifier) => valid_plain(base) && valid_plain(qualifier),
            None => false,
        },
    }
}

fn valid_plain(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-')
}

/// An ordered set of tags.
pub type TagSet = BTreeSet<Tag>;

/// Render a tag set as a stable, space-separated string.
pub fn format_tags(tags: &TagSet) -> String {
    tags.iter()
        .map(Tag::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a list of `key=value` strings into a set.
pub fn parse_tags<I, S>(items: I) -> Result<TagSet, TagError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    items.into_iter().map(|s| Tag::parse(s.as_ref())).collect()
}

#[derive(Debug, Error)]
pub enum TagError {
    #[error("{text:?} is not a valid tag: {reason}")]
    Malformed { text: String, reason: String },

    /// A tag outside the closed vocabulary.
    ///
    /// Carries `suggestions` so callers can render a did-you-mean list. That is
    /// the difference between an agent self-correcting a typo in one turn and
    /// an agent burning ten turns on "no bench matches".
    #[error("unknown tag {tag}{}", suggestion_tail(.suggestions))]
    Unknown { tag: Tag, suggestions: Vec<String> },

    #[error("{0}")]
    Vocabulary(String),
}

fn suggestion_tail(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(" (did you mean: {}?)", suggestions.join(", "))
    }
}

/// One vocabulary entry.
#[derive(Clone, Debug, Default)]
pub struct TagDef {
    pub description: String,
    /// Tags implied by this one, e.g. `soc=esp32s3` implies `family=esp32`.
    pub implies: TagSet,
}

/// A closed set of known tags plus their implication graph.
///
/// Closed vocabularies exist so a typo fails loudly and immediately rather than
/// silently matching nothing. `open_keys` is the escape hatch for keys whose
/// values are inherently unbounded (`name`).
#[derive(Clone, Debug)]
pub struct Vocabulary {
    defs: BTreeMap<Tag, TagDef>,
    open_keys: BTreeSet<String>,
    /// Per-key scarcity weight for best-fit scoring. Identity keys (*which* SoC
    /// this is) describe a bench without being a contended capability, so they
    /// get weight 0; peripherals worth conserving keep weight 1. See
    /// [`crate::matcher::fit_cost`] for why this matters.
    key_weights: BTreeMap<String, f64>,
    /// Prose for each key, surfaced by the `tag_list` tool. Agents match on
    /// descriptions, so these are load-bearing, not decoration.
    key_descriptions: BTreeMap<String, String>,
    /// Keys whose values may carry a free-form `[qualifier]`.
    ///
    /// Opt-in per key, so a qualifier is only legal where the vocabulary says a
    /// value names a *category* with parts underneath it. Without the opt-in
    /// `flash=8mb[whatever]` would quietly become a legal tag that matched
    /// nothing anyone would think to ask for.
    qualified_keys: BTreeSet<String>,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Vocabulary {
            defs: BTreeMap::new(),
            open_keys: ["name".to_string()].into_iter().collect(),
            key_weights: BTreeMap::new(),
            key_descriptions: BTreeMap::new(),
            qualified_keys: BTreeSet::new(),
        }
    }
}

impl Vocabulary {
    pub fn new(
        defs: BTreeMap<Tag, TagDef>,
        open_keys: BTreeSet<String>,
        key_weights: BTreeMap<String, f64>,
        key_descriptions: BTreeMap<String, String>,
        qualified_keys: BTreeSet<String>,
    ) -> Result<Self, TagError> {
        let vocab = Vocabulary {
            defs,
            open_keys,
            key_weights,
            key_descriptions,
            qualified_keys,
        };
        vocab.validate()?;
        Ok(vocab)
    }

    pub fn weight(&self, key: &str) -> f64 {
        self.key_weights.get(key).copied().unwrap_or(1.0)
    }

    pub fn key_weights(&self) -> &BTreeMap<String, f64> {
        &self.key_weights
    }

    pub fn key_description(&self, key: &str) -> Option<&str> {
        self.key_descriptions.get(key).map(String::as_str)
    }

    /// Prose for a tag, falling back to its category.
    ///
    /// A qualified tag has no entry of its own — the whole point is that the
    /// vocabulary does not enumerate parts — so `peripheral=accel[mpu6050]`
    /// borrows the description of `peripheral=accel`. Without this its row in
    /// `tag_list` would be blank, and agents match on those descriptions.
    pub fn describe(&self, tag: &Tag) -> Option<&str> {
        if let Some(def) = self.defs.get(tag) {
            return Some(def.description.as_str());
        }
        if tag.qualifier().is_some() {
            return self
                .defs
                .get(&tag.base_tag())
                .map(|d| d.description.as_str());
        }
        None
    }

    pub fn contains(&self, tag: &Tag) -> bool {
        if self.open_keys.contains(&tag.key) {
            return true;
        }
        if tag.qualifier().is_some() {
            return self.qualified_keys.contains(&tag.key)
                && self.defs.contains_key(&tag.base_tag());
        }
        self.defs.contains_key(tag)
    }

    /// Whether values on this key may carry a `[qualifier]`.
    pub fn is_qualified_key(&self, key: &str) -> bool {
        self.qualified_keys.contains(key)
    }

    pub fn tags(&self) -> impl Iterator<Item = &Tag> {
        self.defs.keys()
    }

    /// Check that vocabulary values are well-formed, that every implied tag
    /// exists, and that there are no cycles.
    fn validate(&self) -> Result<(), TagError> {
        for tag in self.defs.keys() {
            // Dashes are legal in tag values generally (identity values like
            // `name=esp32s3-a` need them), but a *capability* value with a dash
            // is how `esp32-s3` and `esp32s3` end up both existing and matching
            // nothing. Reject it where the vocabulary is declared.
            if tag.value.contains('-') {
                return Err(TagError::Vocabulary(format!(
                    "vocabulary value {tag} must not contain a dash (use {}={} instead)",
                    tag.key,
                    tag.value.replace('-', "")
                )));
            }
            // A vocabulary entry declares a category, never a part. Enumerating
            // parts centrally is exactly what qualifiers exist to avoid.
            if tag.qualifier().is_some() {
                return Err(TagError::Vocabulary(format!(
                    "vocabulary value {tag} must not carry a [qualifier]; declare {}={} \
                     and let benches name the part",
                    tag.key,
                    tag.base()
                )));
            }
        }

        for key in &self.qualified_keys {
            if !self.defs.keys().any(|t| &t.key == key) {
                return Err(TagError::Vocabulary(format!(
                    "key {key:?} is marked qualified but has no values to qualify"
                )));
            }
        }

        for (tag, def) in &self.defs {
            for implied in &def.implies {
                if !self.contains(implied) {
                    return Err(TagError::Vocabulary(format!(
                        "{tag} implies unknown tag {implied}"
                    )));
                }
            }
        }

        // Iterative DFS with colouring; recursion would be fine at this size but
        // a malformed config should never be able to blow the stack.
        #[derive(Clone, Copy, PartialEq)]
        enum Colour {
            White,
            Grey,
            Black,
        }
        let mut colour: BTreeMap<&Tag, Colour> =
            self.defs.keys().map(|t| (t, Colour::White)).collect();

        for root in self.defs.keys() {
            if colour[root] != Colour::White {
                continue;
            }
            let mut stack = vec![(root, false)];
            while let Some((tag, expanded)) = stack.pop() {
                if expanded {
                    colour.insert(tag, Colour::Black);
                    continue;
                }
                colour.insert(tag, Colour::Grey);
                stack.push((tag, true));
                for next in self.defs.get(tag).map(|d| &d.implies).into_iter().flatten() {
                    match colour.get(next) {
                        Some(Colour::Grey) => {
                            return Err(TagError::Vocabulary(format!(
                                "implication cycle involving {tag} -> {next}"
                            )));
                        }
                        Some(Colour::White) => {
                            let key = self.defs.get_key_value(next).unwrap().0;
                            stack.push((key, false));
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Reject anything outside the vocabulary, with did-you-mean suggestions.
    pub fn check<'a, I: IntoIterator<Item = &'a Tag>>(&self, tags: I) -> Result<(), TagError> {
        for tag in tags {
            if let Some(qualifier) = tag.qualifier() {
                if !self.open_keys.contains(&tag.key) && !self.qualified_keys.contains(&tag.key) {
                    return Err(TagError::Vocabulary(format!(
                        "{tag}: values on {:?} do not take a [qualifier]",
                        tag.key
                    )));
                }
                // The same anti-rot rule the vocabulary applies to its own
                // values. A qualifier is matchable, so `accel[mpu-6050]` and
                // `accel[mpu6050]` would be two different parts to a request
                // asking for one of them by name.
                if qualifier.contains('-') {
                    return Err(TagError::Vocabulary(format!(
                        "{tag}: qualifier must not contain a dash (use [{}] instead)",
                        qualifier.replace('-', "")
                    )));
                }
            }
            if !self.contains(tag) {
                return Err(TagError::Unknown {
                    tag: tag.clone(),
                    suggestions: self.suggest(tag, 4),
                });
            }
        }
        Ok(())
    }

    /// Closest known tags, for did-you-mean output.
    pub fn suggest(&self, tag: &Tag, limit: usize) -> Vec<String> {
        // Suggest against the category. The vocabulary holds no parts, so
        // comparing `peripheral=accel[mpu6050]` to it would score every entry
        // badly and offer nothing useful for a mistyped category.
        let tag = &tag.base_tag();
        let target = tag.to_string();
        let mut scored: Vec<(f64, String)> = self
            .defs
            .keys()
            .map(|known| {
                let s = known.to_string();
                (strsim::jaro_winkler(&target, &s), s)
            })
            .filter(|(score, _)| *score >= 0.75)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        if !scored.is_empty() {
            return scored.into_iter().take(limit).map(|(_, s)| s).collect();
        }

        // Unknown value but known key is the common case (`soc=esp32s4`);
        // offering that key's values is more useful than string distance.
        let same_key: Vec<String> = self
            .defs
            .keys()
            .filter(|t| t.key == tag.key)
            .map(Tag::to_string)
            .collect();
        if !same_key.is_empty() {
            return same_key.into_iter().take(limit).collect();
        }

        // Unknown key entirely: offer near-miss keys.
        let mut keys: Vec<(f64, String)> = self
            .defs
            .keys()
            .map(|t| t.key.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|k| (strsim::jaro_winkler(&tag.key, &k), k))
            .filter(|(score, _)| *score >= 0.7)
            .collect();
        keys.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        keys.into_iter().take(limit).map(|(_, k)| k).collect()
    }

    /// Return `tags` plus everything they transitively imply, and the bare
    /// category behind every qualified value.
    ///
    /// Applied to *bench* tags at load time, so an agent asking for
    /// `family=esp32` matches a bench declared only as `soc=esp32s3`.
    /// Requirements are never expanded: doing so would make requests strictly
    /// harder to satisfy, which is the opposite of the intent.
    ///
    /// Qualifiers are desugared here for the same reason, and it is why they
    /// cost the matcher nothing. A bench declaring `peripheral=accel[mpu6050]`
    /// ends up carrying `peripheral=accel` as well, so both a request for the
    /// category and a request for the exact part are satisfied by plain subset
    /// containment. The asymmetry works out too: a *request* for the exact part
    /// is not expanded, so it keeps demanding that part and nothing else.
    pub fn expand(&self, tags: &TagSet) -> TagSet {
        let mut seen = TagSet::new();
        let mut queue: VecDeque<Tag> = tags.iter().cloned().collect();
        while let Some(tag) = queue.pop_front() {
            if !seen.insert(tag.clone()) {
                continue;
            }
            if tag.qualifier().is_some() {
                let base = tag.base_tag();
                if !seen.contains(&base) {
                    queue.push_back(base);
                }
            }
            if let Some(def) = self.defs.get(&tag) {
                for implied in &def.implies {
                    if !seen.contains(implied) {
                        queue.push_back(implied.clone());
                    }
                }
            }
        }
        seen
    }
}
