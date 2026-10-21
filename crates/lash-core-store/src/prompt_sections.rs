//! The recorded vocabulary of prompt sections (FIG-5254, ADR 0133).
//!
//! Every piece of model-facing instruction text is a keyed section owned by
//! the plugin that registered it. Any plugin may wrap any section. The host
//! owns the [`PromptPlan`]: the section order and each section's
//! [`PromptPlacement`]. A plugin's default placement applies only where the
//! plan states none.
//!
//! These are the serialized shapes. The plan is session config, recorded
//! with the config head. A [`ResolvedPromptPlan`] records the order,
//! placements and wrapper chains one model call composed under. A
//! [`PromptSnapshot`] records every section's base text, each wrapper's
//! output and the final text, as content-addressed references. An
//! [`AdmittedRequest`] records the request template a call sends and what
//! reading its response needs (FIG-5259, FIG-5445), beside the call's
//! snapshot until the call settles (FIG-5946). The records are version 1.
//! None is ever read back to recompose a request.

use lash_protocol_types::allocation::{AllocationBudget, AllocationError, AllocationHold};
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::store::BlobRef;

/// The longest local section or wrapper key, in bytes.
pub const PROMPT_KEY_MAX_BYTES: usize = 64;

/// Why a local section or wrapper key is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PromptKeyError {
    #[error("a prompt key is empty")]
    Empty,
    #[error("prompt key `{key}` is longer than the configured {limit}-byte key limit")]
    TooLong { key: String, limit: usize },
    #[error(
        "prompt key `{key}` must start with a lowercase letter or digit and hold only lowercase letters, digits, `_`, `-` and `.`"
    )]
    InvalidCharacter { key: String },
}

fn validate_key(
    key: &str,
    limit: crate::plugin_state::NameByteLimit<64>,
) -> Result<(), PromptKeyError> {
    let Some(first) = key.bytes().next() else {
        return Err(PromptKeyError::Empty);
    };
    if key.len() > limit.max_bytes() {
        return Err(PromptKeyError::TooLong {
            key: key.into(),
            limit: limit.max_bytes(),
        });
    }
    let allowed = |byte: u8| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) || !key.bytes().all(allowed) {
        return Err(PromptKeyError::InvalidCharacter { key: key.into() });
    }
    Ok(())
}

macro_rules! prompt_key {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
            schemars::JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// # Errors
            ///
            /// [`PromptKeyError`] when `key` is empty, too long or holds a
            /// character outside the key alphabet.
            pub fn new(key: impl Into<String>) -> Result<Self, PromptKeyError> {
                Self::with_limit(key, crate::plugin_state::NameByteLimit::standard())
            }
            /// Apply a deployment key-byte limit, at most the 64-byte safety ceiling.
            pub fn with_limit(key: impl Into<String>, limit: crate::plugin_state::NameByteLimit<64>) -> Result<Self, PromptKeyError> {
                let key = key.into();
                validate_key(&key, limit)?;
                Ok(Self(key))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = PromptKeyError;

            fn try_from(key: String) -> Result<Self, Self::Error> {
                Self::new(key)
            }
        }

        impl From<$name> for String {
            fn from(key: $name) -> Self {
                key.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

prompt_key!(
    /// A section's key, local to the plugin that registers it.
    PromptSectionKey
);
prompt_key!(
    /// A wrapper's key, local to the plugin that registers it.
    PromptWrapKey
);

/// A section's identity: the registering plugin and its local key. It names
/// content, not authority: any plugin may wrap any section.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PromptSectionId {
    pub owner: String,
    pub key: PromptSectionKey,
}

impl PromptSectionId {
    pub fn new(owner: impl Into<String>, key: PromptSectionKey) -> Self {
        Self {
            owner: owner.into(),
            key,
        }
    }
}

impl std::fmt::Display for PromptSectionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.key)
    }
}

/// A wrapper's identity: the registering plugin and its local key.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PromptWrapId {
    pub owner: String,
    pub key: PromptWrapKey,
}

impl std::fmt::Display for PromptWrapId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.key)
    }
}

/// Where a section's text reaches the model. Lash sets no policy: the host
/// chooses per section, and a plugin's default applies only where it states
/// none.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PromptPlacement {
    /// The provider's initial instruction field.
    InitialInstructions,
    /// Late in the request, after the projected conversation and outside the
    /// conversation history.
    CurrentContext,
    /// Nowhere: the section is recorded, but neither its renderer nor any
    /// wrapper over it runs, and the request carries none of its text.
    Excluded,
}

/// What a model call is for. A section declares the purposes it renders for.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptPurpose {
    /// A turn's model call.
    Turn,
    /// A compaction's summarizer call.
    Compaction,
    /// A direct completion its owner names.
    Direct { name: String },
}

/// The bounds a composition admits. Oversize is refused, never truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptLimits {
    /// The most sections one call composes.
    pub max_sections: NonZeroU32,
    /// The most wrappers one call applies.
    pub max_wrappers: NonZeroU32,
    /// The most UTF-8 bytes one section's base text or any wrapper's output
    /// holds.
    pub max_section_bytes: NonZeroU32,
    /// The most UTF-8 bytes of final section text one call composes.
    pub max_total_bytes: NonZeroU32,
    /// The wall-clock budget, in milliseconds, for rendering one call,
    /// counted from the call's submission to the shared render pool: time
    /// queued behind other renders counts against it.
    pub render_budget_ms: NonZeroU32,
}

const fn nonzero(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => NonZeroU32::MIN,
    }
}

impl PromptLimits {
    /// Standard preset: 128 sections, 256 wrappers, 32 KiB per section,
    /// 256 KiB total, 2,000 ms rendering. These historical choices have no
    /// universal workload measurement; every plan can override them.
    pub const fn standard() -> Self {
        Self::DEFAULT
    }

    pub const DEFAULT: Self = Self {
        max_sections: nonzero(128),
        max_wrappers: nonzero(256),
        max_section_bytes: nonzero(32 * 1024),
        max_total_bytes: nonzero(256 * 1024),
        render_budget_ms: nonzero(2_000),
    };
}

impl Default for PromptLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The host's placement for one section.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptSectionPlacement {
    pub section: PromptSectionId,
    pub placement: PromptPlacement,
}

/// The host-owned prompt plan, recorded as session config. `order` lists the
/// sections that come first, in that order; every other registered section
/// follows in registration order. `placements` overrides plugin defaults.
/// The empty plan keeps registration order and every plugin default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptPlan {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<PromptSectionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub placements: Vec<PromptSectionPlacement>,
    #[serde(default, skip_serializing_if = "PromptLimits::is_default")]
    pub limits: PromptLimits,
}

impl PromptLimits {
    fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }
}

/// Why a prompt plan is refused.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptPlanError {
    #[error("the plan orders section `{section}` twice")]
    DuplicateOrder { section: PromptSectionId },
    #[error("the plan places section `{section}` twice")]
    DuplicatePlacement { section: PromptSectionId },
    #[error("the plan names section `{section}`, which no installed plugin registers")]
    UnknownSection { section: PromptSectionId },
    /// A section source contributed a section outside its family, or twice,
    /// or one another registration already owns.
    #[error("section source `{family}` contributed `{section}`: {reason}")]
    SourceSectionRefused {
        family: PromptSectionId,
        section: String,
        reason: String,
    },
    #[error("the per-section limit {section} exceeds the total limit {total}")]
    SectionLimitAboveTotal { section: u32, total: u32 },
    #[error("{count} sections exceed the limit of {limit}")]
    TooManySections { count: u32, limit: u32 },
    #[error("{count} wrappers exceed the limit of {limit}")]
    TooManyWrappers { count: u32, limit: u32 },
}

impl PromptPlan {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The host's placement for `section`, if it states one.
    pub fn placement(&self, section: &PromptSectionId) -> Option<PromptPlacement> {
        self.placements
            .iter()
            .find(|placed| &placed.section == section)
            .map(|placed| placed.placement)
    }

    /// Check the plan on its own: no section ordered or placed twice, and a
    /// per-section limit within the total. Whether each named section is
    /// registered is judged when the host sets the plan.
    ///
    /// # Errors
    ///
    /// The first [`PromptPlanError`] the plan breaks.
    pub fn validate(&self) -> Result<(), PromptPlanError> {
        let mut seen = std::collections::BTreeSet::new();
        for section in &self.order {
            if !seen.insert(section) {
                return Err(PromptPlanError::DuplicateOrder {
                    section: section.clone(),
                });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for placed in &self.placements {
            if !seen.insert(&placed.section) {
                return Err(PromptPlanError::DuplicatePlacement {
                    section: placed.section.clone(),
                });
            }
        }
        if self.limits.max_section_bytes > self.limits.max_total_bytes {
            return Err(PromptPlanError::SectionLimitAboveTotal {
                section: self.limits.max_section_bytes.get(),
                total: self.limits.max_total_bytes.get(),
            });
        }
        Ok(())
    }
}

/// Whose choice a resolved placement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlacementSource {
    /// The host's plan stated it.
    Host,
    /// The plan stated none; the registering plugin's default applies.
    PluginDefault,
}

/// One wrapper as a call applies it: its owner's revision, its target and its
/// ordinal in the composition's registration order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPromptWrap {
    pub wrap: PromptWrapId,
    pub owner: String,
    pub target: PromptSectionId,
    /// The wrapper's place in plugin registration order, then declaration
    /// order, across every installed plugin.
    pub ordinal: u32,
}

/// One section as a call composes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPromptSection {
    pub section: PromptSectionId,
    pub owner: String,
    pub placement: PromptPlacement,
    pub placement_source: PlacementSource,
    /// The chain, first applied first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wraps: Vec<ResolvedPromptWrap>,
}

/// The plan one model call composed under, resolved before any renderer ran:
/// the exact order, each placement and whose choice it was, every wrapper
/// chain, and the wrappers that did not run because their target is absent
/// from the call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPromptPlan {
    pub purpose: PromptPurpose,
    pub sections: Vec<ResolvedPromptSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent_targets: Vec<ResolvedPromptWrap>,
    /// Host order or placement overrides whose section is no longer registered,
    /// once per section in key order. They are skipped rather than failing a call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent_overrides: Vec<PromptSectionId>,
    pub limits: PromptLimits,
}

/// A content-addressed reference to section text: equal text shares one
/// stored blob across sections and calls.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptTextRef {
    pub blob: BlobRef,
    pub bytes: u32,
}

impl PromptTextRef {
    /// The reference `text` is stored under.
    pub fn of(text: &str) -> Self {
        Self {
            blob: BlobRef::for_content(text.as_bytes()),
            bytes: u32::try_from(text.len()).unwrap_or(u32::MAX),
        }
    }
}

/// Recorded section text: a reference to exact UTF-8, or an explicit
/// omission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedSectionText {
    Text { text: PromptTextRef },
    Omitted,
}

/// One wrapper's output, as a call recorded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppliedPromptWrap {
    pub wrap: PromptWrapId,
    pub output: RecordedSectionText,
}

/// One section of a call's snapshot: its base text, each applied wrapper's
/// output in chain order, and the final text the request carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderedPromptSection {
    pub section: PromptSectionId,
    pub placement: PromptPlacement,
    pub base: RecordedSectionText,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wraps: Vec<AppliedPromptWrap>,
    pub value: RecordedSectionText,
}

/// The only prompt snapshot version. It encodes as the number 1, and any
/// other number does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "u32")]
pub struct PromptSnapshotVersion;

impl PromptSnapshotVersion {
    pub const NUMBER: u32 = 1;
}

impl TryFrom<u32> for PromptSnapshotVersion {
    type Error = String;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        if version == Self::NUMBER {
            Ok(Self)
        } else {
            Err(format!(
                "prompt snapshot version {version} is not {}",
                Self::NUMBER
            ))
        }
    }
}

impl Serialize for PromptSnapshotVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(Self::NUMBER)
    }
}

impl schemars::JsonSchema for PromptSnapshotVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PromptSnapshotVersion".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "integer", "const": Self::NUMBER })
    }
}

/// The complete prompt one admitted model call carried. It depends on no
/// earlier snapshot, and nothing recomposes a request from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptSnapshot {
    pub version: PromptSnapshotVersion,
    pub plan: ResolvedPromptPlan,
    /// Every selected section, in the plan's order.
    pub sections: Vec<RenderedPromptSection>,
}

/// The most bytes one stored chunk of a request template's literal holds. A
/// chunk ends at a UTF-8 character boundary at or below it, so equal leading
/// bytes of two literals store equal leading chunks.
pub const PROVIDER_BODY_CHUNK_BYTES: usize = 32 * 1024;

/// A content-addressed fragment tree of the exact admitted wire template.
/// Prefix subtrees are shared; an append replaces only the right index path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkedRequestTemplate {
    pub fetch_horizon: lash_protocol_types::llm::attachment_delivery::DeliveryFetchHorizon,
    pub route: lash_protocol_types::llm::types::ProviderRouteIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<lash_protocol_types::llm::types::GenerationReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wire_features: Vec<Box<str>>,
    /// One immutable root, including the ordered attachment slots.
    pub root: PromptTextRef,
    /// Lowered history through its final message, without the closing
    /// envelope. Session ownership keeps this prefix between calls.
    pub prefix: Option<PromptTextRef>,
    /// Request-only envelope nodes, addressed and verified like stored nodes,
    /// but owned directly by this call's record rather than SQL index edges.
    pub inline: Vec<(BlobRef, String)>,
}

/// A stored wire fragment or a binary index node. Its encoded bytes have
/// their own content address; each branch has exactly two edges.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "fragment", rename_all = "snake_case", deny_unknown_fields)]
#[serde(bound(deserialize = "Text: Deserialize<'de>, Slot: Deserialize<'de>"))]
pub enum RequestFragment<Text = String, Slot = Box<lash_protocol_types::llm::types::AttachmentSlot>>
{
    Literal {
        #[cfg_attr(
            any(test, feature = "testing"),
            serde(deserialize_with = "observe_fragment_decode")
        )]
        text: Text,
    },
    Attachment {
        #[cfg_attr(
            any(test, feature = "testing"),
            serde(deserialize_with = "observe_fragment_decode")
        )]
        slot: Slot,
    },
    Branch {
        #[cfg_attr(
            any(test, feature = "testing"),
            serde(deserialize_with = "observe_fragment_decode")
        )]
        left: PromptTextRef,
        right: PromptTextRef,
    },
}

#[cfg(any(test, feature = "testing"))]
std::thread_local! {
    static FRAGMENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "testing"))]
fn observe_fragment_decode<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    FRAGMENT_DECODES.with(|count| count.set(count.get() + 1));
    T::deserialize(deserializer)
}

impl RequestFragment {
    /// Fragment decoder entries on this thread, for admission's no-decode law.
    #[cfg(any(test, feature = "testing"))]
    pub fn decodes_on_current_thread() -> usize {
        FRAGMENT_DECODES.with(std::cell::Cell::get)
    }

    /// Index bodies have a separate content domain from arbitrary audit
    /// text, whose equal spelling must never stand for an installed tree.
    pub fn reference(body: &str) -> PromptTextRef {
        PromptTextRef {
            blob: BlobRef::for_content_parts([
                b"lash.request-fragment/1\0".as_slice(),
                body.as_bytes(),
            ]),
            bytes: u32::try_from(body.len()).unwrap_or(u32::MAX),
        }
    }
}

impl<Text, Slot> RequestFragment<Text, Slot> {
    pub fn children(&self) -> Vec<String> {
        match self {
            Self::Branch { left, right } => vec![
                left.blob.as_str().to_owned(),
                right.blob.as_str().to_owned(),
            ],
            Self::Literal { .. } | Self::Attachment { .. } => Vec::new(),
        }
    }
}

/// One stored prompt text (a section text, or a chunk or index node of a
/// request) under the content address its owner computed. A fresh
/// fragment's builder knows its edges, so admission installs it without
/// decoding it again; the text is shared, so every admission that holds
/// the fragment hands the same allocation to the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptText {
    /// The text's content address.
    pub hash: String,
    /// The exact UTF-8 text.
    pub text: std::sync::Arc<str>,
    /// Outgoing index edges; empty for section text and literal leaves.
    pub children: Vec<String>,
}

#[derive(Clone, Debug)]
struct AdmittedPromptText {
    body: PromptText,
    _hold: AllocationHold,
}
impl std::ops::Deref for AdmittedPromptText {
    type Target = PromptText;
    fn deref(&self) -> &PromptText {
        &self.body
    }
}
impl PartialEq for AdmittedPromptText {
    fn eq(&self, other: &Self) -> bool {
        self.body == other.body
    }
}
impl Eq for AdmittedPromptText {}

#[derive(Clone, Debug)]
struct TextKey(std::sync::Arc<AdmittedPromptText>);
impl std::borrow::Borrow<str> for TextKey {
    fn borrow(&self) -> &str {
        &self.0.hash
    }
}
impl std::hash::Hash for TextKey {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        std::hash::Hash::hash(self.0.hash.as_str(), hasher);
    }
}
impl PartialEq for TextKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.hash == other.0.hash
    }
}
impl Eq for TextKey {}

#[derive(Clone, Debug)]
struct TextUse {
    text: std::sync::Arc<AdmittedPromptText>,
    count: usize,
}
impl PartialEq for TextUse {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}
impl Eq for TextUse {}

/// The texts a request references: the roots its record holds edges to, and
/// every text under them, each once, ordered by content address.
///
/// A turn's history fragments are shared with the session's carried prefix
/// and its roots are known to its builder (FIG-6889), so an admission names
/// them without a pass over the forest, and a store finds a text by address
/// without indexing the forest again.
#[derive(Clone, Debug, Default)]
pub struct RequestTexts {
    roots: Vec<String>,
    texts: crate::persistent_map::PersistentMap<TextKey, TextUse>,
    len: usize,
    delta: Option<std::sync::Arc<[std::sync::Arc<AdmittedPromptText>]>>,
    _metadata: std::sync::Arc<[AllocationHold]>,
}

impl PartialEq for RequestTexts {
    fn eq(&self, other: &Self) -> bool {
        if self.roots != other.roots || self.len != other.len {
            return false;
        }
        let mut equal = true;
        self.texts.for_each(|hash, text| {
            equal &= other.texts.get(hash.0.hash.as_str()) == Some(text);
        });
        equal
    }
}

impl Eq for RequestTexts {}

impl RequestTexts {
    /// The texts of a forest given in any order: each address once, its
    /// roots the texts no other text names as a child.
    pub fn from_texts(texts: impl IntoIterator<Item = PromptText>) -> Self {
        let mut texts = texts
            .into_iter()
            .map(|body| {
                std::sync::Arc::new(AdmittedPromptText {
                    body,
                    _hold: Default::default(),
                })
            })
            .collect::<Vec<_>>();
        texts.sort_by(|left, right| left.hash.cmp(&right.hash));
        texts.dedup_by(|later, earlier| later.hash == earlier.hash);
        let children = texts
            .iter()
            .flat_map(|text| text.children.iter().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>();
        let roots = texts
            .iter()
            .filter(|text| !children.contains(text.hash.as_str()))
            .map(|text| text.hash.clone())
            .collect();
        Self {
            roots,
            len: texts.len(),
            delta: None,
            _metadata: Default::default(),
            texts: texts
                .into_iter()
                .map(|text| (TextKey(text.clone()), TextUse { text, count: 1 }))
                .collect(),
        }
    }

    pub fn retain_allocation(&mut self, hold: AllocationHold) {
        let mut holds = Vec::with_capacity(self._metadata.len() + 1);
        holds.extend(self._metadata.iter().cloned());
        holds.push(hold);
        self._metadata = holds.into();
    }

    /// Changed bodies supplied by an acknowledged persistent owner. Omitted
    /// bodies are external references and must still exist under the writer.
    pub fn supplied_delta(&self) -> Option<impl Iterator<Item = &PromptText>> {
        self.delta
            .as_ref()
            .map(|delta| delta.iter().map(|text| &text.body))
    }

    /// The texts the record roots, in address order.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The text stored under `hash`.
    pub fn get(&self, hash: &str) -> Option<&PromptText> {
        self.texts.get(hash).map(|entry| &entry.text.body)
    }

    /// Every text, in address order.
    pub fn iter(&self) -> impl Iterator<Item = &PromptText> {
        // Whole-forest enumeration is an explicit boundary. Admissions and
        // store lookups keep the shared index; only an enumerator needs its
        // addresses sorted into the public iteration order.
        let mut texts = Vec::with_capacity(self.len);
        self.texts.for_each(|_, entry| texts.push(&entry.text.body));
        texts.sort_unstable_by(|left, right| left.hash.cmp(&right.hash));
        texts.into_iter()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl From<Vec<PromptText>> for RequestTexts {
    fn from(texts: Vec<PromptText>) -> Self {
        Self::from_texts(texts)
    }
}

/// Why a recorded template could not be assembled as admitted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderBodyError {
    #[error("request fragment {hash} does not match its reference")]
    FragmentMismatch { hash: String },
    #[error("request fragment does not decode: {reason}")]
    Index { reason: String },
    /// A chunk the record names is not among the texts read back.
    #[error("request template chunk {hash} is missing")]
    MissingChunk { hash: String },
    /// The assembled segments are not a valid template.
    #[error("the assembled request template is invalid: {0}")]
    Template(#[from] lash_protocol_types::llm::types::TemplateError),
}

/// Split `text` into chunks of at most [`PROVIDER_BODY_CHUNK_BYTES`], each
/// on a character boundary, pushing every chunk's text by its content
/// address onto `texts`. The references are in order.
fn chunk_text(text: &str, texts: &mut Vec<(BlobRef, String)>) -> Vec<PromptTextRef> {
    let mut rest = text;
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        let mut end = rest.len().min(PROVIDER_BODY_CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        let reference = PromptTextRef::of(chunk);
        texts.push((reference.blob.clone(), chunk.to_owned()));
        chunks.push(reference);
        rest = tail;
    }
    chunks
}

type Leaf = (PromptTextRef, std::sync::Arc<AdmittedPromptText>);

/// Store a fragment by its exact encoding, never by re-rendered JSON.
fn encode_fragment(
    fragment: &RequestFragment<&str, &lash_protocol_types::llm::types::AttachmentSlot>,
) -> Leaf {
    let body = serde_json::to_string(fragment).expect("wire fragments encode");
    let reference = RequestFragment::reference(&body);
    let text = PromptText {
        hash: reference.blob.as_str().to_owned(),
        text: body.into(),
        children: fragment.children(),
    };
    (
        reference,
        std::sync::Arc::new(AdmittedPromptText {
            body: text,
            _hold: Default::default(),
        }),
    )
}

fn encode_fragment_owned(
    fragment: &RequestFragment<&str, &lash_protocol_types::llm::types::AttachmentSlot>,
    budget: &AllocationBudget,
) -> Result<Leaf, AllocationError> {
    let hold = budget.reserve(lash_protocol_types::allocation::payload_capacity(
        fragment,
        std::mem::size_of::<AdmittedPromptText>(),
    ))?;
    let (reference, mut text) = encode_fragment(fragment);
    std::sync::Arc::get_mut(&mut text)
        .expect("new fragment is private")
        ._hold = hold;
    Ok((reference, text))
}

fn encode_branch_owned(
    left: &AdmittedPromptText,
    right: &AdmittedPromptText,
    budget: &AllocationBudget,
) -> Result<std::sync::Arc<AdmittedPromptText>, AllocationError> {
    #[derive(Serialize)]
    struct Reference<'a> {
        blob: &'a str,
        bytes: u32,
    }
    #[derive(Serialize)]
    struct Branch<'a> {
        fragment: &'static str,
        left: Reference<'a>,
        right: Reference<'a>,
    }
    fn reference(text: &AdmittedPromptText) -> Reference<'_> {
        Reference {
            blob: text.hash.as_str(),
            bytes: u32::try_from(text.text.len()).unwrap_or(u32::MAX),
        }
    }
    let branch = Branch {
        fragment: "branch",
        left: reference(left),
        right: reference(right),
    };
    let hold = budget.reserve(lash_protocol_types::allocation::payload_capacity(
        &branch,
        std::mem::size_of::<AdmittedPromptText>(),
    ))?;
    // Both references are minted by RequestFragment::reference: two fixed
    // 64-byte digests and u32 lengths fit without a per-branch heap buffer.
    let mut scratch = [0u8; 512];
    let mut writer = std::io::Cursor::new(scratch.as_mut_slice());
    serde_json::to_writer(&mut writer, &branch).expect("fixed-size branch references encode");
    let len = writer.position() as usize;
    let body = std::str::from_utf8(&scratch[..len]).expect("JSON encoding is UTF-8");
    let hash = RequestFragment::reference(body).blob.0;
    let body = PromptText {
        hash,
        text: body.into(),
        children: vec![left.hash.clone(), right.hash.clone()],
    };
    Ok(std::sync::Arc::new(AdmittedPromptText {
        body,
        _hold: hold,
    }))
}

fn keep_fragment(
    fragment: &RequestFragment<&str, &lash_protocol_types::llm::types::AttachmentSlot>,
    texts: &mut Vec<(BlobRef, std::sync::Arc<AdmittedPromptText>)>,
) -> PromptTextRef {
    let (reference, encoded) = encode_fragment(fragment);
    texts.push((reference.blob.clone(), encoded));
    reference
}

/// Splits literal text into leaves: at [`PROVIDER_BODY_CHUNK_BYTES`] from a
/// leaf's start, on UTF-8 boundaries, and after each group of eight escaped
/// line breaks of a long string. In the envelope it also splits the history
/// array into groups of eight messages and finds where the history ends.
struct TextChunker {
    envelope: bool,
    depth: usize,
    history_depth: Option<usize>,
    history_key: bool,
    messages: usize,
    lines: usize,
    quoted: bool,
    escaped: bool,
    /// Leaves before the end of the history array, once it closed.
    prefix_end: Option<usize>,
}

impl TextChunker {
    fn new(envelope: bool) -> Self {
        Self {
            envelope,
            depth: 0,
            history_depth: None,
            history_key: false,
            messages: 0,
            lines: 0,
            quoted: false,
            escaped: false,
            prefix_end: None,
        }
    }

    fn literal_owned(
        &mut self,
        text: &str,
        leaves: &mut Vec<Leaf>,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        let leaf = |text: &str, leaves: &mut Vec<Leaf>| {
            leaves.push(encode_fragment_owned(
                &RequestFragment::Literal { text },
                budget,
            )?);
            Ok::<(), AllocationError>(())
        };
        let mut start = 0;
        let mut string_start = 0;
        for (offset, ch) in text.char_indices() {
            // Oversize messages split from their own start, on UTF-8
            // boundaries, without shifting earlier chunks.
            if offset + ch.len_utf8() - start > PROVIDER_BODY_CHUNK_BYTES {
                leaf(&text[start..offset], leaves)?;
                start = offset;
            }
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                    // A rendered history can occupy one string. Keep short
                    // message bodies intact; completed line groups in a
                    // long string stay reusable.
                    if ch == 'n' {
                        self.lines += 1;
                        if self.lines.is_multiple_of(8)
                            && offset + 1 - string_start >= 4096
                            && offset + 1 - start >= 4096
                        {
                            leaf(&text[start..offset + 1], leaves)?;
                            start = offset + 1;
                        }
                    }
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.quoted = false;
                    if self.envelope && self.depth == 1 {
                        self.history_key = matches!(
                            &text[string_start..offset + 1],
                            "\"messages\"" | "\"contents\"" | "\"input\""
                        );
                    }
                }
            } else if ch == '"' {
                self.quoted = true;
                string_start = offset;
                self.lines = 0;
            } else {
                if ch == ']' && self.history_depth == Some(self.depth) {
                    if start < offset {
                        leaf(&text[start..offset], leaves)?;
                    }
                    start = offset;
                    self.prefix_end = Some(leaves.len());
                    self.history_depth = None;
                } else if ch == ',' && self.history_depth == Some(self.depth) {
                    self.messages += 1;
                    if self.messages.is_multiple_of(8) && offset - start >= 8192 {
                        if start < offset {
                            leaf(&text[start..offset], leaves)?;
                        }
                        // The comma belongs to the next group, so appending
                        // leaves completed groups unchanged.
                        start = offset;
                    }
                }
                if self.envelope && ch == '[' && self.history_key && self.depth == 1 {
                    // The opening envelope rides in the first group rather
                    // than owning another index path.
                    self.history_depth = Some(self.depth + 1);
                    self.history_key = false;
                    self.messages = 0;
                }
                match ch {
                    '{' | '[' => self.depth += 1,
                    '}' | ']' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        if start < text.len() {
            leaf(&text[start..], leaves)?;
        }
        Ok(())
    }
}

fn item_leaves_owned(
    item: &lash_protocol_types::llm::types::RequestSegment,
    separated: bool,
    budget: &AllocationBudget,
) -> Result<Vec<Leaf>, AllocationError> {
    use lash_protocol_types::llm::types::SegmentPart;
    let mut leaves = Vec::new();
    let mut separator = separated;
    for part in item.parts() {
        match part {
            SegmentPart::Literal(text) => {
                let mut chunker = TextChunker::new(false);
                if std::mem::take(&mut separator) {
                    chunker.literal_owned(&format!(",{text}"), &mut leaves, budget)?;
                } else {
                    chunker.literal_owned(text, &mut leaves, budget)?;
                }
            }
            SegmentPart::Slot(slot) => {
                if std::mem::take(&mut separator) {
                    leaves.push(encode_fragment_owned(
                        &RequestFragment::Literal { text: "," },
                        budget,
                    )?);
                }
                leaves.push(encode_fragment_owned(
                    &RequestFragment::Attachment { slot },
                    budget,
                )?);
            }
        }
    }
    Ok(leaves)
}

fn fragment_tree(
    fragments: &[PromptTextRef],
    texts: &mut Vec<(BlobRef, std::sync::Arc<AdmittedPromptText>)>,
) -> PromptTextRef {
    if fragments.len() == 1 {
        return fragments[0].clone();
    }
    let split = 1usize << (fragments.len() - 1).ilog2();
    let left = fragment_tree(&fragments[..split], texts);
    let right = fragment_tree(&fragments[split..], texts);
    keep_fragment(&RequestFragment::Branch { left, right }, texts)
}

mod owner;
pub use owner::OwnerPromptState;

impl ChunkedRequestTemplate {
    /// Read the exact admitted fragments and slots, without any lowerer.
    pub fn assemble<'a>(
        &self,
        text: impl Fn(&BlobRef) -> Option<&'a str>,
    ) -> Result<lash_protocol_types::llm::types::RecordedRequestTemplate, ProviderBodyError> {
        use lash_protocol_types::{
            llm::types::RequestSegment, llm::types::SegmentKind, llm::types::SegmentPart,
        };
        let mut stack = vec![self.root.clone()];
        let mut parts = Vec::new();
        let mut literal = String::new();
        while let Some(reference) = stack.pop() {
            let stored = self
                .inline
                .iter()
                .find(|(hash, _)| hash == &reference.blob)
                .map(|(_, body)| body.as_str())
                .or_else(|| text(&reference.blob))
                .ok_or_else(|| ProviderBodyError::MissingChunk {
                    hash: reference.blob.as_str().to_owned(),
                })?;
            if RequestFragment::reference(stored) != reference {
                return Err(ProviderBodyError::FragmentMismatch {
                    hash: reference.blob.as_str().to_owned(),
                });
            }
            match serde_json::from_str::<RequestFragment>(stored).map_err(|error| {
                ProviderBodyError::Index {
                    reason: error.to_string(),
                }
            })? {
                RequestFragment::Branch { left, right } => {
                    stack.push(right);
                    stack.push(left);
                }
                RequestFragment::Literal { text } => literal.push_str(&text),
                RequestFragment::Attachment { slot } => {
                    if !literal.is_empty() {
                        parts.push(SegmentPart::Literal(std::mem::take(&mut literal).into()));
                    }
                    parts.push(SegmentPart::Slot(slot));
                }
            }
        }
        if !literal.is_empty() {
            parts.push(SegmentPart::Literal(literal.into()));
        }
        // A reopened call is sent as admitted, never lowered again: its
        // body is one text segment.
        let segments = vec![RequestSegment::from_parts(SegmentKind::Text, parts)?];
        let mut template =
            lash_protocol_types::llm::types::RecordedRequestTemplate::from_recorded_segments(
                self.route.clone(),
                self.transport_stream,
                self.generation,
                segments,
            )?;
        template.fetch_horizon = self.fetch_horizon;
        template.wire_features = self.wire_features.clone();
        Ok(template)
    }
}

/// What reading an admitted call's response needs, as its admission records
/// it: the call's scope, and its response contract (the pinned route's
/// model, the output it asked for and its tools' input schemas), addressed
/// in chunks owned directly by the call's admission record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedResponseContext {
    pub scope: lash_protocol_types::llm::types::LlmRequestScope,
    /// The JSON of the call's `ResponseContract`, in order.
    pub contract: Vec<PromptTextRef>,
    /// The request-only contract chunks, kept in the admission record.
    pub inline: Vec<(BlobRef, String)>,
}

impl RecordedResponseContext {
    /// Record the exact response contract beside this call's envelope.
    pub fn record(scope: lash_protocol_types::llm::types::LlmRequestScope, contract: &str) -> Self {
        let mut inline = Vec::new();
        let contract = chunk_text(contract, &mut inline);
        Self {
            scope,
            contract,
            inline,
        }
    }

    /// The contract's JSON, each chunk read from its recorded address.
    ///
    /// # Errors
    ///
    /// [`ProviderBodyError::MissingChunk`] when a chunk is missing; the
    /// caller verified every text against its address.
    pub fn assemble(&self) -> Result<String, ProviderBodyError> {
        let mut contract = String::new();
        for chunk in &self.contract {
            contract.push_str(
                self.inline
                    .iter()
                    .find(|(hash, _)| hash == &chunk.blob)
                    .map(|(_, body)| body.as_str())
                    .ok_or_else(|| ProviderBodyError::MissingChunk {
                        hash: chunk.blob.as_str().to_owned(),
                    })?,
            );
        }
        Ok(contract)
    }
}

/// What one admitted model call sends (ADR 0133 §6), version 1: its request
/// template, what reading its response needs and, for a call no turn row
/// pins, its deadline. A resend reads it back and sends the template with
/// its slots filled afresh, reading the response under the recorded
/// context; nothing recomposes or lowers the call again. It lives until the
/// call settles (FIG-5946); the call's [`PromptSnapshot`] is recorded beside
/// it and outlives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedRequest {
    pub version: PromptSnapshotVersion,
    pub body: ChunkedRequestTemplate,
    pub response: RecordedResponseContext,
    /// The model-total deadline, in milliseconds on the store's clock, that
    /// every send of a compaction's or direct call keeps. A turn's call pins
    /// its deadline in the turn row instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<i64>,
}

#[cfg(test)]
#[path = "prompt_sections_tests.rs"]
mod tests;
