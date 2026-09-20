use crate::ProcessId;
use crate::TurnId;
use crate::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmRole, ProviderReasoningReplay,
    ProviderReplayMeta, ResponseTextMeta,
};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

// ─── Structured message types for context-aware pruning ───

/// A structured message with typed parts for context management.
///
/// `parts` is `Arc`-shared so cloning a `Message` is one Arc bump per
/// message field rather than a deep-clone of every `Part`. Construct with
/// `parts: shared_parts(vec![...])` or `parts: Arc::new(...)`. Mutate via
/// `Arc::make_mut(&mut message.parts)` when truly needed; most plugin
/// pipelines should produce a fresh `Vec<Part>` and assign it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: String,
    pub role: MessageRole,
    pub parts: Arc<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
}

/// Wrap a `Vec<Part>` for the `Message::parts` field so construction sites stay
/// short and uniform.
#[inline]
pub fn shared_parts(parts: Vec<Part>) -> Arc<Vec<Part>> {
    Arc::new(parts)
}

/// A borrowed view of the fields that define message content.
#[derive(Clone, Copy)]
pub(crate) struct MessageContentRef<'a> {
    id: &'a str,
    role: MessageRole,
    parts: &'a Arc<Vec<Part>>,
    origin: Option<&'a MessageOrigin>,
}

impl<'a> From<&'a Message> for MessageContentRef<'a> {
    fn from(message: &'a Message) -> Self {
        let Message {
            id,
            role,
            parts,
            origin,
        } = message;
        Self {
            id: id.as_str(),
            role: *role,
            parts,
            origin: origin.as_ref(),
        }
    }
}

impl<'a> From<&'a super::ConversationRecord> for MessageContentRef<'a> {
    fn from(record: &'a super::ConversationRecord) -> Self {
        let super::ConversationRecord {
            id,
            role,
            parts,
            origin,
        } = record;
        Self {
            id: id.as_str(),
            role: *role,
            parts,
            origin: origin.as_ref(),
        }
    }
}

/// Whether two messages carry the same content.
///
/// This is the predicate the active-read projection asks of every message it
/// reconciles. It compares the message's fields directly: every field is part
/// of the serialized form and none is skipped, so this answers exactly what
/// comparing two `serde_json::Value` trees answered, without building either
/// tree and without a serialization failure mode that could report two
/// different messages as equal.
///
/// `parts` carries the payload and is `Arc`-shared through every projection
/// hop (`ConversationRecord::to_message` clones the pointer, not the parts),
/// so the pointer check settles the common case in constant time regardless
/// of how large the payload is.
pub(crate) fn message_content_equal<'left, 'right>(
    left: impl Into<MessageContentRef<'left>>,
    right: impl Into<MessageContentRef<'right>>,
) -> bool {
    let left = left.into();
    let right = right.into();
    left.id == right.id
        && left.role == right.role
        && left.origin == right.origin
        && (Arc::ptr_eq(left.parts, right.parts) || left.parts == right.parts)
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutputSource {
    Runtime,
    Plugin { plugin_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageOrigin {
    Plugin {
        plugin_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
    },
    Process {
        process_id: ProcessId,
        event_type: String,
        sequence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caused_by: Option<crate::CausalRef>,
    },
    /// The runtime's own commit of a turn's input. A host that renders its own
    /// optimistic user row correlates that row to this committed copy through
    /// `turn_id` — typed provenance the runtime publishes — instead of pinning
    /// or parsing a message id. Message ids stay runtime-minted.
    TurnInput {
        /// The turn whose input this message carries.
        turn_id: TurnId,
        /// The durable turn input this message was materialized from, present
        /// when the input arrived through queued ingress and absent when the
        /// turn was driven with its input in hand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<crate::InputId>,
    },
    /// A durable assistant output belonging to a turn. The source remains
    /// typed so hosts can suppress their live row without parsing ids while
    /// still recognizing protocol-owned output.
    TurnOutput {
        /// The turn that produced this output.
        turn_id: TurnId,
        /// The runtime or plugin that authored the output.
        source: TurnOutputSource,
    },
}

/// A typed message part. Each variant owns exactly the fields its kind
/// may carry — a text part cannot smuggle a `tool_call_id`, and a tool
/// call cannot lack one. `id` is stable identity within the owning
/// `Message` (`{message_id}.p{i}`), `content` is the human-readable or
/// tool-facing text, and `prune_state` tracks lifecycle within the
/// context window.
///
/// Serialization is internally tagged on `kind` so the durable JSON
/// stays the flat, readable shape every stored message already uses
/// (`{"id": …, "kind": "Text", "content": …, "prune_state": …}`).
/// Deserialization accepts that same flat shape and rejects pairings
/// the constructors cannot produce — e.g. a `Text` part carrying a
/// `tool_call_id` — with [`InvalidPartCombination`].
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum Part {
    /// Ordinary response text; `response_meta` carries provider-assigned
    /// phase/replay metadata when present.
    Text {
        id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_meta: Option<ResponseTextMeta>,
        prune_state: PruneState,
    },
    /// A pointer at a stored attachment. Tool-result attachments also
    /// carry the `tool_call_id`/`tool_name` of the call they answer;
    /// `content` holds the placeholder text rendered when the blob is
    /// elided.
    Attachment {
        id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment: Option<PartAttachment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        prune_state: PruneState,
    },
    /// Fenced code block.
    Code {
        id: String,
        content: String,
        prune_state: PruneState,
    },
    /// Tool or process output.
    Output {
        id: String,
        content: String,
        prune_state: PruneState,
    },
    /// An error surfaced as a message part.
    Error {
        id: String,
        content: String,
        prune_state: PruneState,
    },
    /// Markdown prose (e.g. a composed document); `response_meta` carries
    /// provider-assigned phase/replay metadata when present.
    Prose {
        id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_meta: Option<ResponseTextMeta>,
        prune_state: PruneState,
    },
    /// A tool invocation request. `tool_call_id` and `tool_name` are the
    /// provider-side call id and the tool's registered name;
    /// `tool_replay` carries the provider replay token so adapters can
    /// re-emit the call verbatim on the next turn.
    ToolCall {
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_replay: Option<ProviderReplayMeta>,
        prune_state: PruneState,
    },
    /// The text result answering a tool call.
    ToolResult {
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
        prune_state: PruneState,
    },
    /// Chain-of-thought / reasoning item captured from providers that
    /// expose a reasoning channel. `content` holds the human-readable
    /// summary for display (fix 1.3a). The encrypted blob and raw
    /// `summary`/`id` needed to re-feed the model on the next turn
    /// (fix 1.3b) live in `reasoning_meta`. Reasoning parts are preserved
    /// across snapshots so next-turn re-feeding survives session resume;
    /// they are never rendered into the flat chat prompt. Provider
    /// adapters decide whether and how to re-emit them through their
    /// native channel.
    Reasoning {
        id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_meta: Option<ProviderReasoningReplay>,
        prune_state: PruneState,
    },
}

/// A legacy flat `Part` whose `kind` is paired with fields that kind
/// cannot own — e.g. a `Text` part carrying a `tool_call_id`, or a
/// `ToolResult` missing one. [`Part`]'s `Deserialize` impl rejects these
/// pairings with this error instead of silently materializing a state
/// the in-memory type can no longer represent.
#[derive(Clone, Debug, PartialEq)]
pub struct InvalidPartCombination {
    /// The `kind` value the flat form declared.
    pub kind: PartKind,
    /// The field that kind cannot own (or a required field that was
    /// absent, named with a `missing:` prefix).
    pub field: &'static str,
}

impl std::fmt::Display for InvalidPartCombination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "part kind {:?} cannot carry {}", self.kind, self.field)
    }
}

impl std::error::Error for InvalidPartCombination {}

/// The pre-enum flat wire shape of `Part`, kept as the compatibility
/// reader: stored snapshots and externally produced parts decode through
/// it, then are validated into the variant that `kind` selects.
#[derive(serde::Deserialize)]
struct FlatPart {
    id: String,
    kind: PartKind,
    content: String,
    #[serde(default)]
    attachment: Option<PartAttachment>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_replay: Option<ProviderReplayMeta>,
    prune_state: PruneState,
    #[serde(default)]
    reasoning_meta: Option<ProviderReasoningReplay>,
    #[serde(default)]
    response_meta: Option<ResponseTextMeta>,
}

impl FlatPart {
    /// The first field outside the set `kind` may carry, if any.
    fn invalid_field(&self) -> Option<&'static str> {
        use PartKind::*;
        let tainted = [
            (
                "attachment",
                self.attachment.is_some(),
                self.kind == Attachment,
            ),
            (
                "tool_call_id",
                self.tool_call_id.is_some(),
                matches!(self.kind, Attachment | ToolCall | ToolResult),
            ),
            (
                "tool_name",
                self.tool_name.is_some(),
                matches!(self.kind, Attachment | ToolCall | ToolResult),
            ),
            (
                "tool_replay",
                self.tool_replay.is_some(),
                self.kind == ToolCall,
            ),
            (
                "reasoning_meta",
                self.reasoning_meta.is_some(),
                self.kind == Reasoning,
            ),
            (
                "response_meta",
                self.response_meta.is_some(),
                matches!(self.kind, Text | Prose),
            ),
        ];
        tainted
            .into_iter()
            .find_map(|(field, present, valid)| (present && !valid).then_some(field))
    }

    fn try_into_part(self) -> Result<Part, InvalidPartCombination> {
        if let Some(field) = self.invalid_field() {
            return Err(InvalidPartCombination {
                kind: self.kind,
                field,
            });
        }
        let missing = |field: &'static str| InvalidPartCombination {
            kind: self.kind,
            field,
        };
        let part = match self.kind {
            PartKind::Text => Part::Text {
                id: self.id,
                content: self.content,
                response_meta: self.response_meta,
                prune_state: self.prune_state,
            },
            PartKind::Attachment => {
                // A tool-result attachment carries the call pair; an
                // ordinary one carries neither. A lone id or name is
                // a pairing the constructors cannot produce.
                let (tool_call_id, tool_name) = match (self.tool_call_id, self.tool_name) {
                    (Some(_), None) => return Err(missing("missing:tool_name")),
                    (None, Some(_)) => return Err(missing("missing:tool_call_id")),
                    pair => pair,
                };
                Part::Attachment {
                    id: self.id,
                    content: self.content,
                    attachment: self.attachment,
                    tool_call_id,
                    tool_name,
                    prune_state: self.prune_state,
                }
            }
            PartKind::Code => Part::Code {
                id: self.id,
                content: self.content,
                prune_state: self.prune_state,
            },
            PartKind::Output => Part::Output {
                id: self.id,
                content: self.content,
                prune_state: self.prune_state,
            },
            PartKind::Error => Part::Error {
                id: self.id,
                content: self.content,
                prune_state: self.prune_state,
            },
            PartKind::Prose => Part::Prose {
                id: self.id,
                content: self.content,
                response_meta: self.response_meta,
                prune_state: self.prune_state,
            },
            PartKind::ToolCall => {
                let (tool_call_id, tool_name) = match (self.tool_call_id, self.tool_name) {
                    (Some(call_id), Some(name)) => (call_id, name),
                    (None, _) => return Err(missing("missing:tool_call_id")),
                    (_, None) => return Err(missing("missing:tool_name")),
                };
                Part::ToolCall {
                    id: self.id,
                    content: self.content,
                    tool_call_id,
                    tool_name,
                    tool_replay: self.tool_replay,
                    prune_state: self.prune_state,
                }
            }
            PartKind::ToolResult => {
                let (tool_call_id, tool_name) = match (self.tool_call_id, self.tool_name) {
                    (Some(call_id), Some(name)) => (call_id, name),
                    (None, _) => return Err(missing("missing:tool_call_id")),
                    (_, None) => return Err(missing("missing:tool_name")),
                };
                Part::ToolResult {
                    id: self.id,
                    content: self.content,
                    tool_call_id,
                    tool_name,
                    prune_state: self.prune_state,
                }
            }
            PartKind::Reasoning => Part::Reasoning {
                id: self.id,
                content: self.content,
                reasoning_meta: self.reasoning_meta,
                prune_state: self.prune_state,
            },
        };
        Ok(part)
    }
}

impl<'de> serde::Deserialize<'de> for Part {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <FlatPart as serde::Deserialize>::deserialize(deserializer)?
            .try_into_part()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PartKind {
    Text,
    Attachment,
    Code,
    Output,
    Error,
    Prose,
    ToolCall,
    ToolResult,
    /// Chain-of-thought / reasoning item captured from providers that expose
    /// a reasoning channel. `content` holds the human-readable summary for
    /// display (fix 1.3a). The encrypted blob and raw `summary`/`id` needed
    /// to re-feed the model on the next turn (fix 1.3b) live in
    /// `reasoning_meta`. Reasoning parts are preserved across snapshots so
    /// next-turn re-feeding survives session resume; they are never rendered
    /// into the flat chat prompt. Provider adapters decide whether and how
    /// to re-emit them through their native channel.
    Reasoning,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartAttachment {
    pub source: AttachmentSource,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PruneState {
    Intact,
    Cleared,
    Deleted {
        breadcrumb: String,
        archive_hash: String,
    },
    Summarized {
        summary: String,
        archive_hash: String,
    },
}

impl Part {
    /// Test-only constructor used by fixtures that only need a kind and
    /// content; tool variants get placeholder call metadata.
    #[cfg(test)]
    fn base(id: String, kind: PartKind, content: String) -> Self {
        let prune_state = PruneState::Intact;
        match kind {
            PartKind::Text => Self::Text {
                id,
                content,
                response_meta: None,
                prune_state,
            },
            PartKind::Attachment => Self::Attachment {
                id,
                content,
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                prune_state,
            },
            PartKind::Code => Self::Code {
                id,
                content,
                prune_state,
            },
            PartKind::Output => Self::Output {
                id,
                content,
                prune_state,
            },
            PartKind::Error => Self::Error {
                id,
                content,
                prune_state,
            },
            PartKind::Prose => Self::Prose {
                id,
                content,
                response_meta: None,
                prune_state,
            },
            PartKind::ToolCall => Self::ToolCall {
                id,
                content,
                tool_call_id: String::new(),
                tool_name: String::new(),
                tool_replay: None,
                prune_state,
            },
            PartKind::ToolResult => Self::ToolResult {
                id,
                content,
                tool_call_id: String::new(),
                tool_name: String::new(),
                prune_state,
            },
            PartKind::Reasoning => Self::Reasoning {
                id,
                content,
                reasoning_meta: None,
                prune_state,
            },
        }
    }

    /// Stable identity within the owning `Message` (`{message_id}.p{i}`).
    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Attachment { id, .. }
            | Self::Code { id, .. }
            | Self::Output { id, .. }
            | Self::Error { id, .. }
            | Self::Prose { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. }
            | Self::Reasoning { id, .. } => id,
        }
    }

    /// Mutable access to the part's stable identity — used when a sequence
    /// renames ids to the `{message_id}.p{i}` convention.
    pub fn id_mut(&mut self) -> &mut String {
        match self {
            Self::Text { id, .. }
            | Self::Attachment { id, .. }
            | Self::Code { id, .. }
            | Self::Output { id, .. }
            | Self::Error { id, .. }
            | Self::Prose { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. }
            | Self::Reasoning { id, .. } => id,
        }
    }

    /// Which payload contract this part follows.
    pub fn kind(&self) -> PartKind {
        match self {
            Self::Text { .. } => PartKind::Text,
            Self::Attachment { .. } => PartKind::Attachment,
            Self::Code { .. } => PartKind::Code,
            Self::Output { .. } => PartKind::Output,
            Self::Error { .. } => PartKind::Error,
            Self::Prose { .. } => PartKind::Prose,
            Self::ToolCall { .. } => PartKind::ToolCall,
            Self::ToolResult { .. } => PartKind::ToolResult,
            Self::Reasoning { .. } => PartKind::Reasoning,
        }
    }

    /// Human-readable or tool-facing text; attachments may carry the
    /// placeholder text rendered when the blob is elided.
    pub fn content(&self) -> &str {
        match self {
            Self::Text { content, .. }
            | Self::Attachment { content, .. }
            | Self::Code { content, .. }
            | Self::Output { content, .. }
            | Self::Error { content, .. }
            | Self::Prose { content, .. }
            | Self::ToolCall { content, .. }
            | Self::ToolResult { content, .. }
            | Self::Reasoning { content, .. } => content,
        }
    }

    /// Mutable access to the rendered text — used by pruning and recovery
    /// paths that rewrite part content in place.
    pub fn content_mut(&mut self) -> &mut String {
        match self {
            Self::Text { content, .. }
            | Self::Attachment { content, .. }
            | Self::Code { content, .. }
            | Self::Output { content, .. }
            | Self::Error { content, .. }
            | Self::Prose { content, .. }
            | Self::ToolCall { content, .. }
            | Self::ToolResult { content, .. }
            | Self::Reasoning { content, .. } => content,
        }
    }

    /// Lifecycle within the context window: intact, cleared, deleted
    /// (breadcrumb retained), or summarized.
    pub fn prune_state(&self) -> &PruneState {
        match self {
            Self::Text { prune_state, .. }
            | Self::Attachment { prune_state, .. }
            | Self::Code { prune_state, .. }
            | Self::Output { prune_state, .. }
            | Self::Error { prune_state, .. }
            | Self::Prose { prune_state, .. }
            | Self::ToolCall { prune_state, .. }
            | Self::ToolResult { prune_state, .. }
            | Self::Reasoning { prune_state, .. } => prune_state,
        }
    }

    /// Mutable access to the prune lifecycle — pruning plugins rewrite
    /// content and state together.
    pub fn prune_state_mut(&mut self) -> &mut PruneState {
        match self {
            Self::Text { prune_state, .. }
            | Self::Attachment { prune_state, .. }
            | Self::Code { prune_state, .. }
            | Self::Output { prune_state, .. }
            | Self::Error { prune_state, .. }
            | Self::Prose { prune_state, .. }
            | Self::ToolCall { prune_state, .. }
            | Self::ToolResult { prune_state, .. }
            | Self::Reasoning { prune_state, .. } => prune_state,
        }
    }

    /// The stored attachment this part points at; `Some` only for
    /// attachment parts that carry one.
    pub fn attachment(&self) -> Option<&PartAttachment> {
        match self {
            Self::Attachment { attachment, .. } => attachment.as_ref(),
            _ => None,
        }
    }

    /// Mutable access to the attachment slot; `None` for non-attachment
    /// parts. Lets prune paths elide the pointer while leaving the
    /// placeholder `content` in place.
    pub fn attachment_mut(&mut self) -> Option<&mut Option<PartAttachment>> {
        match self {
            Self::Attachment { attachment, .. } => Some(attachment),
            _ => None,
        }
    }

    /// The provider-side call id this part's call issues or result
    /// answers; `Some` for tool calls, tool results, and tool-result
    /// attachments.
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::Attachment { tool_call_id, .. } => tool_call_id.as_deref(),
            Self::ToolCall { tool_call_id, .. } | Self::ToolResult { tool_call_id, .. } => {
                Some(tool_call_id)
            }
            _ => None,
        }
    }

    /// The invoked tool's registered name; `Some` for the same parts as
    /// [`Part::tool_call_id`].
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::Attachment { tool_name, .. } => tool_name.as_deref(),
            Self::ToolCall { tool_name, .. } | Self::ToolResult { tool_name, .. } => {
                Some(tool_name)
            }
            _ => None,
        }
    }

    /// Provider replay token on a tool call; `Some` only for tool-call
    /// parts that carry one.
    pub fn tool_replay(&self) -> Option<&ProviderReplayMeta> {
        match self {
            Self::ToolCall { tool_replay, .. } => tool_replay.as_ref(),
            _ => None,
        }
    }

    /// Provider reasoning replay payload; `Some` only for reasoning
    /// parts that carry one.
    pub fn reasoning_meta(&self) -> Option<&ProviderReasoningReplay> {
        match self {
            Self::Reasoning { reasoning_meta, .. } => reasoning_meta.as_ref(),
            _ => None,
        }
    }

    /// Provider-assigned phase/replay metadata for response text;
    /// `Some` only for text and prose parts that carry one.
    pub fn response_meta(&self) -> Option<&ResponseTextMeta> {
        match self {
            Self::Text { response_meta, .. } | Self::Prose { response_meta, .. } => {
                response_meta.as_ref()
            }
            _ => None,
        }
    }

    pub fn text(id: String, content: String, response_meta: Option<ResponseTextMeta>) -> Self {
        Self::Text {
            id,
            content,
            response_meta,
            prune_state: PruneState::Intact,
        }
    }

    pub fn attachment_part(
        id: String,
        content: String,
        attachment: Option<PartAttachment>,
    ) -> Self {
        Self::Attachment {
            id,
            content,
            attachment,
            tool_call_id: None,
            tool_name: None,
            prune_state: PruneState::Intact,
        }
    }

    pub fn tool_result_attachment(
        id: String,
        content: String,
        attachment: PartAttachment,
        tool_call_id: String,
        tool_name: String,
    ) -> Self {
        Self::Attachment {
            id,
            content,
            attachment: Some(attachment),
            tool_call_id: Some(tool_call_id),
            tool_name: Some(tool_name),
            prune_state: PruneState::Intact,
        }
    }

    pub fn code(id: String, content: String) -> Self {
        Self::Code {
            id,
            content,
            prune_state: PruneState::Intact,
        }
    }

    pub fn output(id: String, content: String) -> Self {
        Self::Output {
            id,
            content,
            prune_state: PruneState::Intact,
        }
    }

    pub fn error(id: String, content: String) -> Self {
        Self::Error {
            id,
            content,
            prune_state: PruneState::Intact,
        }
    }

    pub fn prose(id: String, content: String, response_meta: Option<ResponseTextMeta>) -> Self {
        Self::Prose {
            id,
            content,
            response_meta,
            prune_state: PruneState::Intact,
        }
    }

    pub fn tool_call(
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
        tool_replay: Option<ProviderReplayMeta>,
    ) -> Self {
        Self::ToolCall {
            id,
            content,
            tool_call_id,
            tool_name,
            tool_replay,
            prune_state: PruneState::Intact,
        }
    }

    pub fn tool_result(
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
    ) -> Self {
        Self::ToolResult {
            id,
            content,
            tool_call_id,
            tool_name,
            prune_state: PruneState::Intact,
        }
    }

    pub fn reasoning(
        id: String,
        content: String,
        reasoning_meta: Option<ProviderReasoningReplay>,
    ) -> Self {
        Self::Reasoning {
            id,
            content,
            reasoning_meta,
            prune_state: PruneState::Intact,
        }
    }

    #[cfg(test)]
    pub(crate) fn prompt_char_count(&self) -> usize {
        // Reasoning parts are not user-visible text and aren't sent to the
        // model as flat prompt content. Provider adapters may re-emit them
        // via structured replay metadata instead. Excluding them from the
        // accounting keeps the rolling-history plugin's prune decisions
        // driven by real conversation content.
        if matches!(self.kind(), PartKind::Reasoning) {
            return 0;
        }
        if matches!(self.kind(), PartKind::Attachment) {
            return self
                .attachment()
                .and_then(|attachment| attachment.source.stored_ref())
                .map(|attachment_ref| attachment_ref.id.as_str().len())
                .unwrap_or_else(|| self.render().len());
        }
        self.render().len()
    }

    pub(crate) fn render(&self) -> String {
        if let Self::Attachment {
            attachment,
            content,
            ..
        } = self
        {
            return if attachment.is_some() || content.trim().is_empty() {
                "[Attachment]".to_string()
            } else {
                content.clone()
            };
        }
        match self.prune_state() {
            PruneState::Intact => self.content().to_string(),
            PruneState::Cleared => "[Old tool result content cleared]".to_string(),
            PruneState::Deleted {
                breadcrumb,
                archive_hash,
            } => format!("[pruned:{} — {}]", archive_hash, breadcrumb),
            PruneState::Summarized {
                summary,
                archive_hash,
            } => format!("[SUMMARY of original {}]\n{}", archive_hash, summary),
        }
    }
}

impl Message {
    pub fn is_transient(&self) -> bool {
        matches!(
            self.origin,
            Some(MessageOrigin::Plugin {
                transient: true,
                ..
            })
        )
    }
}

fn render_part_for_chat(role: MessageRole, part: &Part) -> String {
    let rendered = part.render();
    match role {
        MessageRole::System => match part.kind() {
            PartKind::Code => rendered,
            PartKind::Output => format!("<output>\n{}\n</output>", rendered),
            PartKind::Error => format!("<error>\n{}\n</error>", rendered),
            PartKind::Text
            | PartKind::Attachment
            | PartKind::Prose
            | PartKind::ToolCall
            | PartKind::ToolResult
            | PartKind::Reasoning => rendered,
        },
        MessageRole::Assistant => match part.kind() {
            PartKind::Code => rendered,
            PartKind::ToolCall => render_assistant_tool_call(part, &rendered),
            PartKind::Prose | PartKind::Text | PartKind::Attachment | PartKind::ToolResult => {
                rendered
            }
            PartKind::Reasoning => rendered,
            _ => rendered,
        },
        MessageRole::User | MessageRole::Event => rendered,
    }
}

fn render_assistant_tool_call(part: &Part, rendered: &str) -> String {
    let tool_name = part.tool_name().unwrap_or("tool");
    let trimmed = rendered.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        format!("{tool_name}()")
    } else {
        format!("{tool_name}({trimmed})")
    }
}

fn attachment_from_part(part: &Part) -> Option<AttachmentSource> {
    if !matches!(part.kind(), PartKind::Attachment) {
        return None;
    }
    let attachment = part.attachment()?;
    Some(attachment.source.clone())
}

fn render_message_for_transcript(msg: &Message, attachments: &mut Vec<AttachmentSource>) -> String {
    let mut out = Vec::new();
    for part in msg.parts.iter() {
        // Reasoning items are display-only from the transcript's point of
        // view — they are never replayed as flat text. Provider adapters use
        // structured replay metadata when they can re-emit reasoning.
        if matches!(part.kind(), PartKind::Reasoning) {
            continue;
        }
        if let Some(attachment) = attachment_from_part(part) {
            attachments.push(attachment);
            out.push("[Attachment]".to_string());
            continue;
        }
        let rendered = render_part_for_chat(msg.role, part);
        if !rendered.trim().is_empty() {
            out.push(rendered);
        }
    }
    out.join("\n\n")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub messages: Vec<LlmMessage>,
}

impl RenderedPrompt {
    /// Sources in message order, derived from the structured blocks.
    pub fn attachments(&self) -> Vec<&AttachmentSource> {
        self.messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Attachment { source } => Some(source.as_ref()),
                _ => None,
            })
            .collect()
    }
}

/// Memoized render of a `MessageSequence`'s `base`. Shared across the
/// per-iteration `MessageSequence` instances that wrap the same base
/// (typically the `SessionGraphCache`'s projected messages) so the
/// chat projector's `render_prompt` walk happens once per turn instead
/// of once per LLM iteration.
pub type BaseRenderCache = OnceLock<RenderedPrompt>;

/// How a `MessageSequence` stores its messages.
///
/// `Layered` is the base/delta rope: a session-wide `base` shared by `Arc`
/// plus the messages this iteration appended. `Owned` is a flat list the
/// sequence holds outright — the shape every sequence settles into after
/// `make_mut`/`replace`, and the only shape a deserialized sequence can
/// have. Layered-with-owned is not representable.
#[derive(Debug)]
enum SequenceMode {
    Layered {
        base: Arc<Vec<Message>>,
        delta: Vec<Message>,
    },
    Owned(Vec<Message>),
}

#[derive(Debug)]
pub struct MessageSequence {
    mode: SequenceMode,
    materialized: OnceLock<Arc<Vec<Message>>>,
    base_rendered: Option<Arc<BaseRenderCache>>,
}

impl Clone for MessageSequence {
    fn clone(&self) -> Self {
        let mode = match &self.mode {
            SequenceMode::Layered { base, delta } => SequenceMode::Layered {
                base: Arc::clone(base),
                delta: delta.clone(),
            },
            SequenceMode::Owned(owned) => SequenceMode::Owned(owned.clone()),
        };
        Self {
            mode,
            materialized: OnceLock::new(),
            base_rendered: self.base_rendered.as_ref().map(Arc::clone),
        }
    }
}

impl Default for MessageSequence {
    fn default() -> Self {
        Self::from_owned(Vec::new())
    }
}

impl From<Vec<Message>> for MessageSequence {
    fn from(messages: Vec<Message>) -> Self {
        Self::from_owned(messages)
    }
}

// A `MessageSequence` is a memoized base/delta rope with caches; its meaningful
// value is the flat, materialized message list. Serialize as exactly that list
// (and reconstruct an owned sequence on the way back) so that types embedding a
// `MessageSequence` can derive serde with the same wire form as a plain
// `Vec<Message>`. This is what lets `Effect` be serialized directly in a turn
// checkpoint instead of round-tripping through a parallel `Vec<Message>` twin.
impl serde::Serialize for MessageSequence {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for MessageSequence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let messages = Vec::<Message>::deserialize(deserializer)?;
        Ok(Self::from_owned(messages))
    }
}

impl std::ops::Deref for MessageSequence {
    type Target = [Message];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl MessageSequence {
    pub(crate) fn from_owned(messages: Vec<Message>) -> Self {
        Self {
            mode: SequenceMode::Owned(messages),
            materialized: OnceLock::new(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base(base: Arc<Vec<Message>>) -> Self {
        Self {
            mode: SequenceMode::Layered {
                base,
                delta: Vec::new(),
            },
            materialized: OnceLock::new(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base_and_delta(base: Arc<Vec<Message>>, delta: Vec<Message>) -> Self {
        Self {
            mode: SequenceMode::Layered { base, delta },
            materialized: OnceLock::new(),
            base_rendered: None,
        }
    }

    /// Attach a shared render cache for the `base` portion. Subsequent
    /// `render_prompt` calls will reuse the memoized `RenderedPrompt` for
    /// the base instead of rewalking it. The delta is always re-rendered
    /// because it changes per LLM iteration. Returns `self` for chaining.
    pub(crate) fn with_base_render_cache(mut self, cache: Arc<BaseRenderCache>) -> Self {
        self.base_rendered = Some(cache);
        self
    }

    pub(crate) fn len(&self) -> usize {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.len(),
            SequenceMode::Layered { base, delta } => base.len() + delta.len(),
        }
    }

    /// The messages `next` adds on top of this sequence, when `next` extends
    /// this sequence without rewriting any of its prefix.
    ///
    /// The agreement is *witnessed by construction* rather than computed: a
    /// base/delta rope whose `base` is the same allocation as this sequence's
    /// `base` shares that prefix by identity, so deciding is a pointer
    /// comparison plus a walk of the (turn-sized) delta — never a walk of the
    /// session's history.
    ///
    /// Returns `None` when the witness is absent: either side rebuilt itself
    /// into an owned list (`make_mut`/`replace`, i.e. a plugin deliberately
    /// rewrote history), the bases are different allocations, or `next`'s
    /// delta diverges from this one's. `None` means "cannot decide cheaply",
    /// so callers fall back to reconciling content.
    pub(crate) fn preserved_extension_delta<'a>(&self, next: &'a Self) -> Option<&'a [Message]> {
        let (
            SequenceMode::Layered {
                base: self_base,
                delta: self_delta,
            },
            SequenceMode::Layered {
                base: next_base,
                delta: next_delta,
            },
        ) = (&self.mode, &next.mode)
        else {
            return None;
        };
        if !Arc::ptr_eq(self_base, next_base) {
            return None;
        }
        let tail = next_delta.get(self_delta.len()..)?;
        self_delta
            .iter()
            .zip(next_delta.iter())
            .all(|(current, candidate)| message_content_equal(current, candidate))
            .then_some(tail)
    }

    pub(crate) fn iter(&self) -> MessageSequenceIter<'_> {
        match &self.mode {
            SequenceMode::Owned(owned) => MessageSequenceIter::Owned(owned.iter()),
            SequenceMode::Layered { base, delta } => {
                MessageSequenceIter::Split(base.iter().chain(delta.iter()))
            }
        }
    }

    /// The flattened message list as a shared allocation. `Owned` wraps its
    /// list once; `Layered` reuses `base` when the delta is empty and joins
    /// base and delta otherwise.
    fn materialize(&self) -> &Arc<Vec<Message>> {
        match &self.mode {
            SequenceMode::Owned(owned) => self.materialized.get_or_init(|| Arc::new(owned.clone())),
            SequenceMode::Layered { base, delta } if delta.is_empty() => base,
            SequenceMode::Layered { base, delta } => self.materialized.get_or_init(|| {
                let mut combined = Vec::with_capacity(base.len() + delta.len());
                combined.extend(base.iter().cloned());
                combined.extend(delta.iter().cloned());
                Arc::new(combined)
            }),
        }
    }

    pub(crate) fn as_slice(&self) -> &[Message] {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.as_slice(),
            SequenceMode::Layered { .. } => self.materialize().as_slice(),
        }
    }

    pub(crate) fn shared(&self) -> Arc<Vec<Message>> {
        Arc::clone(self.materialize())
    }

    pub fn make_mut(&mut self) -> &mut Vec<Message> {
        if matches!(self.mode, SequenceMode::Layered { .. }) {
            let owned = self.materialize_owned();
            self.mode = SequenceMode::Owned(owned);
        }
        self.materialized = OnceLock::new();
        match &mut self.mode {
            SequenceMode::Owned(owned) => owned,
            SequenceMode::Layered { .. } => unreachable!("mode was just set to Owned"),
        }
    }

    /// The layered sequence's flattened list as an owned `Vec`, reusing the
    /// materialized cache or the `base` allocation when either already holds
    /// it exclusively.
    fn materialize_owned(&self) -> Vec<Message> {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.clone(),
            SequenceMode::Layered { base, delta } if delta.is_empty() => {
                Arc::unwrap_or_clone(Arc::clone(base))
            }
            SequenceMode::Layered { .. } => Arc::unwrap_or_clone(Arc::clone(self.materialize())),
        }
    }

    pub(crate) fn push(&mut self, message: Message) {
        match &mut self.mode {
            SequenceMode::Owned(owned) => owned.push(message),
            SequenceMode::Layered { delta, .. } => delta.push(message),
        }
        self.materialized = OnceLock::new();
    }

    pub(crate) fn extend(&mut self, messages: Vec<Message>) {
        if messages.is_empty() {
            return;
        }
        match &mut self.mode {
            SequenceMode::Owned(owned) => owned.extend(messages),
            SequenceMode::Layered { delta, .. } => delta.extend(messages),
        }
        self.materialized = OnceLock::new();
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.mode = SequenceMode::Owned(messages);
        self.materialized = OnceLock::new();
    }

    pub(crate) fn render_prompt(&self) -> RenderedPrompt {
        let SequenceMode::Layered { base, delta } = &self.mode else {
            return render_prompt(self.as_slice());
        };
        if base.is_empty() {
            return render_prompt(delta.as_slice());
        }
        let mut rendered = match &self.base_rendered {
            Some(cache) => cache.get_or_init(|| render_prompt(base.as_slice())).clone(),
            None => render_prompt(base.as_slice()),
        };
        if !delta.is_empty() {
            append_rendered_prompt(&mut rendered, delta.as_slice());
        }
        rendered
    }
}

pub enum MessageSequenceIter<'a> {
    Owned(std::slice::Iter<'a, Message>),
    Split(std::iter::Chain<std::slice::Iter<'a, Message>, std::slice::Iter<'a, Message>>),
}

impl<'a> Iterator for MessageSequenceIter<'a> {
    type Item = &'a Message;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Owned(iter) => iter.next(),
            Self::Split(iter) => iter.next(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TranscriptTurn {
    user: Vec<String>,
    assistant: Vec<String>,
}

pub fn render_prompt(msgs: &[Message]) -> RenderedPrompt {
    let mut rendered = RenderedPrompt::default();
    append_rendered_prompt(&mut rendered, msgs);
    rendered
}

pub fn messages_are_prompt_resume_safe<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> bool {
    let mut seen_tool_calls = HashSet::new();
    let mut completed_tool_calls = HashSet::new();

    for message in messages {
        for part in message.parts.iter() {
            // Reasoning parts don't participate in tool pairing and are
            // always safe to resume through.
            if matches!(part.kind(), PartKind::Reasoning) {
                continue;
            }
            match part.kind() {
                PartKind::ToolCall => {
                    if !matches!(message.role, MessageRole::Assistant) {
                        return false;
                    }
                    let Some(call_id) = part
                        .tool_call_id()
                        .map(str::trim)
                        .filter(|call_id| !call_id.is_empty())
                    else {
                        return false;
                    };
                    if !seen_tool_calls.insert(call_id) {
                        return false;
                    }
                }
                PartKind::ToolResult => {
                    if !matches!(message.role, MessageRole::User) {
                        return false;
                    }
                    let Some(call_id) = part
                        .tool_call_id()
                        .map(str::trim)
                        .filter(|call_id| !call_id.is_empty())
                    else {
                        return false;
                    };
                    if !seen_tool_calls.contains(call_id) {
                        return false;
                    }
                    if !completed_tool_calls.insert(call_id) {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }

    seen_tool_calls.len() == completed_tool_calls.len()
}

pub fn render_transcript_prompt(msgs: &[Message]) -> RenderedPrompt {
    let mut attachments = Vec::new();
    let mut turns = Vec::new();
    let mut current = TranscriptTurn::default();
    let mut has_current = false;

    for msg in msgs {
        let text = render_message_for_transcript(msg, &mut attachments);
        let has_text = !text.trim().is_empty();
        match msg.role {
            MessageRole::User | MessageRole::Event => {
                if has_current && (!current.user.is_empty() || !current.assistant.is_empty()) {
                    turns.push(current);
                    current = TranscriptTurn::default();
                }
                if has_text {
                    current
                        .user
                        .push(if matches!(msg.role, MessageRole::Event) {
                            format!("Event:\n{text}")
                        } else {
                            text
                        });
                }
                has_current = true;
            }
            MessageRole::Assistant | MessageRole::System => {
                if !has_current {
                    has_current = true;
                }
                if has_text {
                    current.assistant.push(text);
                }
            }
        }
    }

    if has_current && (!current.user.is_empty() || !current.assistant.is_empty()) {
        turns.push(current);
    }

    let mut text = String::new();
    text.push_str(
        "History:\nThis is a chronological transcript. `Assistant` refers to Lash, and you are continuing the same session.\n\n",
    );
    for (idx, turn) in turns.iter().enumerate() {
        text.push_str(&format!("=== Turn {} ===\n", idx + 1));
        text.push_str("User:\n");
        if turn.user.is_empty() {
            text.push_str("[No user content recorded]\n");
        } else {
            text.push_str(&turn.user.join("\n\n"));
            text.push('\n');
        }
        text.push('\n');
        text.push_str("Assistant (Lash, continuing this transcript):\n");
        let is_current_pending_turn = idx + 1 == turns.len() && turn.assistant.is_empty();
        if turn.assistant.is_empty() && !is_current_pending_turn {
            text.push_str("[No assistant content recorded]\n");
        } else if !turn.assistant.is_empty() {
            text.push_str(&turn.assistant.join("\n\n"));
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str(
        "Continue from the latest turn as Lash.\nIf the task is complete, provide the final answer.\nOtherwise produce the next valid step for this runtime.",
    );

    let mut message = LlmMessage::text(LlmRole::User, text);
    Arc::make_mut(&mut message.blocks).extend(attachments.into_iter().map(|source| {
        LlmContentBlock::Attachment {
            source: Box::new(source),
        }
    }));
    RenderedPrompt {
        messages: vec![message],
    }
}

pub fn append_rendered_prompt(rendered: &mut RenderedPrompt, msgs: &[Message]) {
    append_structured_prompt(rendered, msgs)
}

#[cfg(test)]
fn render_structured_prompt(msgs: &[Message]) -> RenderedPrompt {
    let mut rendered = RenderedPrompt::default();
    append_structured_prompt(&mut rendered, msgs);
    rendered
}

fn append_structured_prompt(rendered: &mut RenderedPrompt, msgs: &[Message]) {
    for msg in msgs {
        let mut blocks: Vec<LlmContentBlock> = Vec::new();
        for part in msg.parts.iter() {
            match part.kind() {
                PartKind::Reasoning => {
                    let Some(meta) = part.reasoning_meta() else {
                        continue;
                    };
                    if meta.is_empty() {
                        continue;
                    }
                    blocks.push(LlmContentBlock::Reasoning {
                        text: part.content().to_string(),
                        replay: Some(meta.clone()),
                    });
                }
                PartKind::ToolCall => {
                    let call_id = part.tool_call_id().unwrap_or_default().to_string();
                    let tool_name = part.tool_name().unwrap_or_default().to_string();
                    blocks.push(LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json: part.content().to_string(),
                        replay: part.tool_replay().cloned(),
                    });
                }
                PartKind::ToolResult => {
                    let text = part.render();
                    let call_id = part.tool_call_id().unwrap_or_default().to_string();
                    blocks.push(LlmContentBlock::ToolResult {
                        call_id,
                        content: text,
                        tool_name: part.tool_name().map(str::to_string),
                    });
                }
                _ => {
                    if let Some(attachment) = attachment_from_part(part)
                        && matches!(msg.role, MessageRole::User)
                    {
                        blocks.push(LlmContentBlock::Attachment {
                            source: Box::new(attachment),
                        });
                        continue;
                    }

                    let mut text = render_part_for_chat(msg.role, part);
                    if text.trim().is_empty() {
                        continue;
                    }

                    if matches!(msg.role, MessageRole::System | MessageRole::Event) {
                        text = if matches!(msg.role, MessageRole::Event) {
                            format!("Runtime event:\n{text}")
                        } else {
                            format!("Runtime note:\n{text}")
                        };
                    }

                    blocks.push(LlmContentBlock::Text {
                        text: text.into(),
                        response_meta: part.response_meta().cloned(),
                        cache_breakpoint: false,
                    });
                }
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let mut projected = LlmMessage::new(llm_role_for_message(msg.role), blocks);
        projected.starts_user_segment = matches!(
            (msg.role, msg.origin.as_ref()),
            (MessageRole::User, Some(MessageOrigin::TurnInput { .. }))
        );
        rendered.messages.push(projected);
    }
}

fn llm_role_for_message(role: MessageRole) -> LlmRole {
    match role {
        MessageRole::User => LlmRole::User,
        MessageRole::Assistant => LlmRole::Assistant,
        MessageRole::System => LlmRole::System,
        MessageRole::Event => LlmRole::User,
    }
}

#[cfg(test)]
mod replay_provenance_tests;
#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
