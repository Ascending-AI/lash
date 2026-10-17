//! The streamed output block: its kind, its provider-minted identity and
//! the one lifecycle payload every lane carries for it.

use schemars::JsonSchema;

/// Which render lane a streamed output block belongs to.
///
/// A *block* is the unit a host renders: one OpenAI reasoning summary part,
/// one Anthropic `thinking`/`text` content block, or one ordinal run of
/// thought/text for providers with no native notion. Blocks name their owning
/// reasoning item or message item when the provider has one; several blocks
/// may share one item's replay material.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StreamBlockKind {
    /// Visible assistant prose.
    AssistantText,
    /// Reasoning summary ("thinking") text, kept separate from assistant
    /// response text.
    Reasoning,
}

/// Provider-minted identity of one streamed output block.
///
/// Minted by the provider adapter from provider facts: OpenAI
/// `(item_id, summary_index)`, Anthropic content-block index, or a
/// deterministic per-attempt ordinal for providers with no native notion.
/// The id is opaque to hosts; ordering comes from `ordinal`, never from
/// parsing `id`. Live and replayed streams emit identical identities.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct StreamBlockIdentity {
    /// Opaque provider-minted block key, stable within one response attempt.
    pub id: String,
    /// Per-attempt block ordinal assigned by the provider adapter.
    pub ordinal: u64,
    /// The reasoning or message item this block belongs to, when the provider
    /// has one. N blocks may share one item's replay material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

impl StreamBlockIdentity {
    pub fn new(id: impl Into<String>, ordinal: u64) -> Self {
        Self {
            id: id.into(),
            ordinal,
            item_id: None,
        }
    }

    pub fn with_item_id(mut self, item_id: Option<String>) -> Self {
        self.item_id = item_id.filter(|id| !id.is_empty());
        self
    }
}

/// One step of a streamed output block's lifecycle: the single payload every
/// lane carries for it — the provider stream, the session stream, turn
/// activity and the trace. A lane adds its own envelope (observation
/// identity, correlation, sequence, elapsed time) around this payload and
/// never redefines it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum StreamBlockEvent {
    /// A provider-minted block opened. Blocks are the unit hosts render;
    /// merging adjacent blocks is the host's choice — Lash injects no
    /// separators.
    Started {
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
    },
    /// The next suffix of `block`'s text.
    Delta {
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
        text: String,
    },
    /// `block` closed. `text` is the block's authoritative text, so a
    /// consumer can correct drift from accumulated deltas.
    Completed {
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
        text: String,
    },
}

impl StreamBlockEvent {
    pub fn started(kind: StreamBlockKind, block: StreamBlockIdentity) -> Self {
        Self::Started { kind, block }
    }

    pub fn delta(
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
        text: impl Into<String>,
    ) -> Self {
        Self::Delta {
            kind,
            block,
            text: text.into(),
        }
    }

    pub fn completed(
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
        text: impl Into<String>,
    ) -> Self {
        Self::Completed {
            kind,
            block,
            text: text.into(),
        }
    }

    pub fn kind(&self) -> StreamBlockKind {
        match self {
            Self::Started { kind, .. }
            | Self::Delta { kind, .. }
            | Self::Completed { kind, .. } => *kind,
        }
    }

    pub fn block(&self) -> &StreamBlockIdentity {
        match self {
            Self::Started { block, .. }
            | Self::Delta { block, .. }
            | Self::Completed { block, .. } => block,
        }
    }

    /// The delta's suffix, when this event is a delta.
    pub fn delta_text(&self) -> Option<&str> {
        match self {
            Self::Delta { text, .. } => Some(text),
            Self::Started { .. } | Self::Completed { .. } => None,
        }
    }
}
