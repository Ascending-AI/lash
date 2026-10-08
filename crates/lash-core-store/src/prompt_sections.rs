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
//! [`AdmittedModelCall`] records a call's snapshot with its request template
//! (FIG-5259, FIG-5445). The records are version 1. None is ever read back to
//! recompose a request.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::store::BlobRef;
use crate::store::plugin_writers::PluginRevision;
use lash_sansio::llm::types::AttachmentSlot;

/// The longest local section or wrapper key, in bytes.
pub const PROMPT_KEY_MAX_BYTES: usize = 64;

/// Why a local section or wrapper key is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PromptKeyError {
    #[error("a prompt key is empty")]
    Empty,
    #[error("prompt key `{key}` is longer than {PROMPT_KEY_MAX_BYTES} bytes")]
    TooLong { key: String },
    #[error(
        "prompt key `{key}` must start with a lowercase letter or digit and hold only lowercase letters, digits, `_`, `-` and `.`"
    )]
    InvalidCharacter { key: String },
}

fn validate_key(key: &str) -> Result<(), PromptKeyError> {
    let Some(first) = key.bytes().next() else {
        return Err(PromptKeyError::Empty);
    };
    if key.len() > PROMPT_KEY_MAX_BYTES {
        return Err(PromptKeyError::TooLong { key: key.into() });
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
                let key = key.into();
                validate_key(&key)?;
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
    pub owner: PluginRevision,
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
    pub owner: PluginRevision,
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

/// The request template an admitted call sends, as its record stores it
/// (ADR 0133 §6, ADR 0135 §6): the route that lowered it, whether it
/// streams, the generation receipt its provider built, and its segments in
/// wire order. A literal is stored as content-addressed chunks of its own,
/// so a template that shares a literal prefix with an earlier call's shares
/// that prefix's chunks. A slot is recorded inline: its ref, position,
/// acceptance and codec, never a delivered value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkedRequestTemplate {
    pub route: lash_sansio::llm::types::ProviderRouteIdentity,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<lash_sansio::llm::types::GenerationReceipt>,
    /// Every segment, in wire order.
    pub segments: Vec<ChunkedSegment>,
}

/// One stored segment of a [`ChunkedRequestTemplate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "segment", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChunkedSegment {
    /// Literal JSON text: its length in bytes and every chunk, in order.
    Literal {
        bytes: u64,
        chunks: Vec<PromptTextRef>,
    },
    /// One attachment slot, filled per attempt.
    Attachment {
        slot: lash_sansio::llm::types::AttachmentSlot,
    },
}

/// Why a recorded template could not be assembled as admitted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderBodyError {
    /// A chunk the record names is not among the texts read back.
    #[error("request template chunk {hash} is missing")]
    MissingChunk { hash: String },
    /// An assembled literal is not its recorded length.
    #[error(
        "an assembled request template literal holds {assembled} bytes, not the recorded {recorded}"
    )]
    Length { assembled: u64, recorded: u64 },
    /// The assembled segments are not a valid template.
    #[error("the assembled request template is invalid: {0}")]
    Template(#[from] lash_sansio::llm::types::TemplateError),
}

impl ChunkedRequestTemplate {
    /// Record `template` as chunks: the record and each literal chunk's text
    /// by its content address.
    pub fn chunk(
        template: &lash_sansio::llm::types::RecordedRequestTemplate,
    ) -> (Self, Vec<(BlobRef, String)>) {
        use lash_sansio::llm::types::RequestSegment;
        let mut texts = Vec::new();
        let segments = template
            .segments
            .iter()
            .map(|segment| match segment {
                RequestSegment::Literal { text } => {
                    let mut rest = &**text;
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
                    ChunkedSegment::Literal {
                        bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
                        chunks,
                    }
                }
                RequestSegment::Attachment { slot } => ChunkedSegment::Attachment {
                    slot: AttachmentSlot::clone(slot),
                },
            })
            .collect();
        (
            Self {
                route: template.route.clone(),
                stream: template.stream,
                generation: template.generation,
                segments,
            },
            texts,
        )
    }

    /// The template these segments hold, each literal chunk read from
    /// `text` by its address, validated as a template.
    ///
    /// # Errors
    ///
    /// [`ProviderBodyError`] when a chunk is missing, a literal is not its
    /// recorded length, or the segments are not a valid template; the caller
    /// verified every text against its address.
    pub fn assemble<'a>(
        &self,
        text: impl Fn(&BlobRef) -> Option<&'a str>,
    ) -> Result<lash_sansio::llm::types::RecordedRequestTemplate, ProviderBodyError> {
        use lash_sansio::llm::types::RequestSegment;
        let mut segments = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            segments.push(match segment {
                ChunkedSegment::Literal { bytes, chunks } => {
                    let mut literal = String::new();
                    for chunk in chunks {
                        let stored =
                            text(&chunk.blob).ok_or_else(|| ProviderBodyError::MissingChunk {
                                hash: chunk.blob.as_str().to_owned(),
                            })?;
                        literal.push_str(stored);
                    }
                    let assembled = u64::try_from(literal.len()).unwrap_or(u64::MAX);
                    if assembled != *bytes {
                        return Err(ProviderBodyError::Length {
                            assembled,
                            recorded: *bytes,
                        });
                    }
                    RequestSegment::Literal {
                        text: literal.into(),
                    }
                }
                ChunkedSegment::Attachment { slot } => RequestSegment::Attachment {
                    slot: Box::new(slot.clone()),
                },
            });
        }
        let template = lash_sansio::llm::types::RecordedRequestTemplate {
            route: self.route.clone(),
            stream: self.stream,
            generation: self.generation,
            segments,
        };
        template.validate()?;
        Ok(template)
    }
}

/// What one admitted model call commits (ADR 0133 §6), version 1: its
/// prompt snapshot, its request template and, for a call no turn row pins,
/// its deadline. A resend reads it back and sends the template with its
/// slots filled afresh; nothing recomposes or lowers the call again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedModelCall {
    pub version: PromptSnapshotVersion,
    /// The call's prompt; `None` when the session registers no sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptSnapshot>,
    pub body: ChunkedRequestTemplate,
    /// The model-total deadline, in milliseconds on the store's clock, that
    /// every send of a compaction's or direct call keeps. A turn's call pins
    /// its deadline in the turn row instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<i64>,
}

#[cfg(test)]
#[path = "prompt_sections_tests.rs"]
mod tests;
