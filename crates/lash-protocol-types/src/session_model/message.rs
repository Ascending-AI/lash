use crate::AttachmentRef;
use crate::ToolCallId;
use crate::TurnId;
use crate::append_vec::AppendVec;
use crate::llm::types::{
    LlmContentBlock, LlmMessage, LlmRole, ProviderReasoningReplay, ProviderReplayMeta,
    ResponseTextMeta,
};
use crate::tool_output::ModelToolReturnPart;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

// ─── Structured message types for context-aware pruning ───

/// A structured message with typed parts for context management.
///
/// `parts` is `Arc`-shared so cloning a `Message` is one Arc bump per message field rather
/// than a deep-clone of every `Part`.
/// Mutate via `Arc::make_mut(&mut message.parts)` when truly needed; most plugin pipelines
/// should produce a fresh `Vec<Part>` and assign it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: String,
    pub role: MessageRole,
    pub parts: Arc<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    /// Present on the one message that is its turn's reply: the runtime mints
    /// it at the turn's commit, for every way a turn finishes, and nothing
    /// else can (FIG-1493 §5.1). Orthogonal to `origin`, which keeps naming
    /// who wrote the message. A message committed before the marker existed
    /// has none, and that absence is history, never backfilled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_marker: Option<TurnReply>,
}

/// Marks a message as the reply of one turn and names the part that carries
/// the reply's text, so a reasoning-then-prose message resolves to its prose.
///
/// One turn has at most one reply: the runtime keys the mint on the turn,
/// never on the commit attempt, so a redriven or re-executed turn finds its
/// reply and mints no second one. Only the runtime constructs a marker; a
/// host or plugin reads it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnReply {
    turn_id: TurnId,
    part_id: String,
}

impl TurnReply {
    /// The turn this message replies for.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// The part of this message that carries the reply.
    pub fn part_id(&self) -> &str {
        &self.part_id
    }

    pub(crate) fn mint(turn_id: TurnId, part_id: String) -> Self {
        Self { turn_id, part_id }
    }
}

#[inline]
pub fn shared_parts(parts: Vec<Part>) -> Arc<Vec<Part>> {
    Arc::new(parts)
}

/// A borrowed view of the fields that define message content.
#[derive(Clone, Copy)]
pub struct MessageContentRef<'a> {
    id: &'a str,
    role: MessageRole,
    parts: &'a Arc<Vec<Part>>,
    origin: Option<&'a MessageOrigin>,
    reply_marker: Option<&'a TurnReply>,
}

impl<'a> From<&'a Message> for MessageContentRef<'a> {
    fn from(message: &'a Message) -> Self {
        let Message {
            id,
            role,
            parts,
            origin,
            reply_marker,
        } = message;
        Self {
            id: id.as_str(),
            role: *role,
            parts,
            origin: origin.as_ref(),
            reply_marker: reply_marker.as_ref(),
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
            reply_marker,
        } = record;
        Self {
            id: id.as_str(),
            role: *role,
            parts,
            origin: origin.as_ref(),
            reply_marker: reply_marker.as_ref(),
        }
    }
}

/// Whether `existing` and `value` are the same message, for
/// [`AppendVec::push_adopting`]: a sequence that appends a message another
/// holder of its buffer already appended adopts that slot.
pub fn same_message(existing: &Message, value: &Message) -> bool {
    message_content_equal(existing, value)
}

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
pub fn message_content_equal<'left, 'right>(
    left: impl Into<MessageContentRef<'left>>,
    right: impl Into<MessageContentRef<'right>>,
) -> bool {
    let left = left.into();
    let right = right.into();
    left.id == right.id
        && left.role == right.role
        && left.origin == right.origin
        && left.reply_marker == right.reply_marker
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
#[non_exhaustive]
pub enum MessageOrigin {
    Plugin {
        plugin_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
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
        /// turn was executed with its input in hand.
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
        /// The executed cell this output is the assistant context of: the
        /// [`crate::CellRecord::id`] of the cell the reply ran. `None` for
        /// output that ran no cell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_id: Option<String>,
    },
}

/// An ordered message part whose variant owns its kind-specific fields.
/// `id` identifies the part within its message; `content` is rendered text,
/// except on a tool result, whose content is its ordered blocks.
/// Serialization uses a flat object and rejects unknown fields and invalid
/// kind/field combinations through [`InvalidPartCombination`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Part {
    /// Ordinary response text; `response_meta` carries provider-assigned
    /// phase/replay metadata when present.
    Text {
        id: String,
        content: String,
        response_meta: Option<ResponseTextMeta>,
    },
    /// A pointer at a stored attachment; `content` holds the placeholder
    /// text rendered when the blob is elided. An attachment a tool returned
    /// is a block inside that call's [`Part::ToolResult`], never a part of
    /// its own.
    Attachment {
        id: String,
        content: String,
        attachment: Option<PartAttachment>,
    },
    /// Fenced code block.
    Code { id: String, content: String },
    /// Tool or process output.
    Output { id: String, content: String },
    /// An error surfaced as a message part.
    Error { id: String, content: String },
    /// Markdown prose (e.g. a composed document); `response_meta` carries
    /// provider-assigned phase/replay metadata when present.
    Prose {
        id: String,
        content: String,
        response_meta: Option<ResponseTextMeta>,
    },
    /// A tool invocation request. `call_id` is lash's identity for the
    /// call (ADR 0117); `provider_call_id` is the provider's correlation,
    /// which the adapter re-emits and nothing else reads. `tool_name` is the
    /// tool's registered name; `tool_replay` carries the provider replay
    /// token so adapters can re-emit the call verbatim on the next turn.
    ToolCall {
        id: String,
        content: String,
        call_id: ToolCallId,
        provider_call_id: String,
        tool_name: String,
        tool_replay: Option<ProviderReplayMeta>,
    },
    /// The one result answering a tool call: its text and attachment
    /// blocks in the order the tool's value produced them. A call is
    /// answered by exactly one result part, whatever its value holds. It
    /// pairs with its call by `call_id`; an adapter answers the provider
    /// with the paired call's correlation.
    ToolResult {
        id: String,
        status: crate::ToolCallStatus,
        content: Vec<ModelToolReturnPart>,
        call_id: ToolCallId,
        tool_name: String,
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
        reasoning_meta: Option<ProviderReasoningReplay>,
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

/// Strict flat wire shape, validated into the variant selected by `kind`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FlatPart {
    id: String,
    kind: PartKind,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    blocks: Option<Vec<ModelToolReturnPart>>,
    #[serde(default)]
    status: Option<crate::ToolCallStatus>,
    #[serde(default)]
    attachment: Option<PartAttachment>,
    #[serde(default)]
    call_id: Option<ToolCallId>,
    #[serde(default)]
    provider_call_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_replay: Option<ProviderReplayMeta>,
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
            ("content", self.content.is_some(), self.kind != ToolResult),
            ("blocks", self.blocks.is_some(), self.kind == ToolResult),
            ("status", self.status.is_some(), self.kind == ToolResult),
            (
                "attachment",
                self.attachment.is_some(),
                self.kind == Attachment,
            ),
            (
                "call_id",
                self.call_id.is_some(),
                matches!(self.kind, ToolCall | ToolResult),
            ),
            (
                "provider_call_id",
                self.provider_call_id.is_some(),
                self.kind == ToolCall,
            ),
            (
                "tool_name",
                self.tool_name.is_some(),
                matches!(self.kind, ToolCall | ToolResult),
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
        let tool_pair = |call_id: Option<ToolCallId>, name: Option<String>| match (call_id, name) {
            (Some(call_id), Some(name)) => Ok((call_id, name)),
            (None, _) => Err(missing("missing:call_id")),
            (_, None) => Err(missing("missing:tool_name")),
        };
        let text = |content: Option<String>| content.ok_or_else(|| missing("missing:content"));
        let part = match self.kind {
            PartKind::Text => Part::Text {
                id: self.id,
                content: text(self.content)?,
                response_meta: self.response_meta,
            },
            PartKind::Attachment => Part::Attachment {
                id: self.id,
                content: text(self.content)?,
                attachment: self.attachment,
            },
            PartKind::Code => Part::Code {
                id: self.id,
                content: text(self.content)?,
            },
            PartKind::Output => Part::Output {
                id: self.id,
                content: text(self.content)?,
            },
            PartKind::Error => Part::Error {
                id: self.id,
                content: text(self.content)?,
            },
            PartKind::Prose => Part::Prose {
                id: self.id,
                content: text(self.content)?,
                response_meta: self.response_meta,
            },
            PartKind::ToolCall => {
                let (call_id, tool_name) = tool_pair(self.call_id, self.tool_name)?;
                Part::ToolCall {
                    id: self.id,
                    content: text(self.content)?,
                    call_id,
                    provider_call_id: self
                        .provider_call_id
                        .ok_or_else(|| missing("missing:provider_call_id"))?,
                    tool_name,
                    tool_replay: self.tool_replay,
                }
            }
            PartKind::ToolResult => {
                let (call_id, tool_name) = tool_pair(self.call_id, self.tool_name)?;
                Part::ToolResult {
                    status: self.status.ok_or_else(|| missing("missing:status"))?,
                    id: self.id,
                    content: self.blocks.ok_or_else(|| missing("missing:blocks"))?,
                    call_id,
                    tool_name,
                }
            }
            PartKind::Reasoning => Part::Reasoning {
                id: self.id,
                content: text(self.content)?,
                reasoning_meta: self.reasoning_meta,
            },
        };
        Ok(part)
    }
}

/// The borrowed counterpart of [`FlatPart`]. [`Part`]'s `Serialize` impl
/// writes through it: a tool result writes its `blocks` and no `content`,
/// every other kind writes its `content` string.
#[derive(serde::Serialize)]
struct FlatPartRef<'a> {
    id: &'a str,
    kind: PartKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<&'a [ModelToolReturnPart]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<crate::ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachment: Option<&'a PartAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<&'a ToolCallId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_replay: Option<&'a ProviderReplayMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_meta: Option<&'a ProviderReasoningReplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_meta: Option<&'a ResponseTextMeta>,
}

impl serde::Serialize for Part {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        FlatPartRef {
            id: self.id(),
            kind: self.kind(),
            content: self.text_content(),
            blocks: self.tool_result_content(),
            status: self.tool_result_status(),
            attachment: self.attachment(),
            call_id: self.call_id(),
            provider_call_id: self.provider_call_id(),
            tool_name: self.tool_name(),
            tool_replay: self.tool_replay(),
            reasoning_meta: self.reasoning_meta(),
            response_meta: self.response_meta(),
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Part {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <FlatPart as serde::Deserialize>::deserialize(deserializer)?
            .try_into_part()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartKind {
    Text,
    Attachment,
    Internal(InternalPartKind),
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

/// An opaque discriminator for protocol-owned committed parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InternalPartKind(InternalPartDiscriminator);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InternalPartDiscriminator {
    Code,
    Output,
    Error,
}

#[allow(non_upper_case_globals)]
impl PartKind {
    pub(crate) const Code: Self = Self::Internal(InternalPartKind(InternalPartDiscriminator::Code));
    pub(crate) const Output: Self =
        Self::Internal(InternalPartKind(InternalPartDiscriminator::Output));
    pub(crate) const Error: Self =
        Self::Internal(InternalPartKind(InternalPartDiscriminator::Error));
}
impl serde::Serialize for PartKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match *self {
            Self::Text => "Text",
            Self::Attachment => "Attachment",
            Self::Code => "Code",
            Self::Output => "Output",
            Self::Error => "Error",
            Self::Prose => "Prose",
            Self::ToolCall => "ToolCall",
            Self::ToolResult => "ToolResult",
            Self::Reasoning => "Reasoning",
        })
    }
}
impl<'de> serde::Deserialize<'de> for PartKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <String as serde::Deserialize>::deserialize(deserializer)?;
        match name.as_str() {
            "Text" => Ok(Self::Text),
            "Attachment" => Ok(Self::Attachment),
            "Code" => Ok(Self::Code),
            "Output" => Ok(Self::Output),
            "Error" => Ok(Self::Error),
            "Prose" => Ok(Self::Prose),
            "ToolCall" => Ok(Self::ToolCall),
            "ToolResult" => Ok(Self::ToolResult),
            "Reasoning" => Ok(Self::Reasoning),
            unknown => Err(serde::de::Error::unknown_variant(
                unknown,
                &[
                    "Text",
                    "Attachment",
                    "Code",
                    "Output",
                    "Error",
                    "Prose",
                    "ToolCall",
                    "ToolResult",
                    "Reasoning",
                ],
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartAttachment {
    pub reference: AttachmentRef,
}

impl Part {
    #[cfg(test)]
    fn base(id: String, kind: PartKind, content: String) -> Self {
        match kind {
            PartKind::Text => Self::Text {
                id,
                content,
                response_meta: None,
            },
            PartKind::Attachment => Self::Attachment {
                id,
                content,
                attachment: None,
            },
            PartKind::Code => Self::Code { id, content },
            PartKind::Output => Self::Output { id, content },
            PartKind::Error => Self::Error { id, content },
            PartKind::Prose => Self::Prose {
                id,
                content,
                response_meta: None,
            },
            PartKind::ToolCall => Self::ToolCall {
                id,
                content,
                call_id: ToolCallId::fixture("base"),
                provider_call_id: String::new(),
                tool_name: String::new(),
                tool_replay: None,
            },
            PartKind::ToolResult => Self::ToolResult {
                id,
                status: crate::ToolCallStatus::Success,
                content: vec![ModelToolReturnPart::text(content)],
                call_id: ToolCallId::fixture("base"),
                tool_name: String::new(),
            },
            PartKind::Reasoning => Self::Reasoning {
                id,
                content,
                reasoning_meta: None,
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
    /// placeholder text rendered when the blob is elided. A tool result
    /// joins its visible text blocks, leaving its attachments typed; attachments
    /// have no textual substitute in this accessor.
    pub fn content(&self) -> Cow<'_, str> {
        match self {
            Self::ToolResult { content, .. } => tool_result_visible_text(content),
            _ => Cow::Borrowed(self.text_content().unwrap_or_default()),
        }
    }

    /// The part's own text string; `None` for a tool result, whose content
    /// is its ordered blocks.
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::Text { content, .. }
            | Self::Attachment { content, .. }
            | Self::Code { content, .. }
            | Self::Output { content, .. }
            | Self::Error { content, .. }
            | Self::Prose { content, .. }
            | Self::ToolCall { content, .. }
            | Self::Reasoning { content, .. } => Some(content),
            Self::ToolResult { .. } => None,
        }
    }

    /// Mutable access to the part's text — used by pruning and recovery
    /// paths that rewrite part content in place. `None` for a tool result;
    /// rewrite its blocks through [`Part::tool_result_content_mut`].
    pub fn content_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Text { content, .. }
            | Self::Attachment { content, .. }
            | Self::Code { content, .. }
            | Self::Output { content, .. }
            | Self::Error { content, .. }
            | Self::Prose { content, .. }
            | Self::ToolCall { content, .. }
            | Self::Reasoning { content, .. } => Some(content),
            Self::ToolResult { .. } => None,
        }
    }

    /// The terminal status on a tool result. Full causes stay in the recorded call.
    pub fn tool_result_status(&self) -> Option<crate::ToolCallStatus> {
        match self {
            Self::ToolResult { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A tool result's ordered text and attachment blocks; `None` for every other kind.
    pub fn tool_result_content(&self) -> Option<&[ModelToolReturnPart]> {
        match self {
            Self::ToolResult { content, .. } => Some(content),
            _ => None,
        }
    }

    /// Mutable access to a tool result's blocks, for paths that elide an
    /// attachment or shorten text inside the one result.
    pub fn tool_result_content_mut(&mut self) -> Option<&mut Vec<ModelToolReturnPart>> {
        match self {
            Self::ToolResult { content, .. } => Some(content),
            _ => None,
        }
    }

    /// Every attachment ref the part carries, in content order: an
    /// attachment part's pointer, or the attachment blocks of a tool result.
    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        let own = self.attachment().map(|attachment| &attachment.reference);
        let blocks = self
            .tool_result_content()
            .unwrap_or_default()
            .iter()
            .filter_map(ModelToolReturnPart::attachment);
        own.into_iter().chain(blocks)
    }

    /// Every attachment ref the part carries, each with an id unique
    /// within the message: an attachment part's own id, or `{id}#{n}` for
    /// the tool result's `n`th attachment (1-based, matching its
    /// `[Attachment n]` marker).
    pub fn identified_attachments(&self) -> Vec<(String, &AttachmentRef)> {
        match self {
            Self::ToolResult { id, .. } => self
                .attachments()
                .enumerate()
                .map(|(index, source)| (format!("{id}#{}", index + 1), source))
                .collect(),
            _ => self
                .attachments()
                .map(|source| (self.id().to_string(), source))
                .collect(),
        }
    }

    /// Mutable access to every attachment source the part carries, in the
    /// order [`Part::attachments`] yields them.
    pub fn attachments_mut(&mut self) -> Vec<&mut AttachmentRef> {
        match self {
            Self::Attachment {
                attachment: Some(attachment),
                ..
            } => vec![&mut attachment.reference],
            Self::ToolResult { content, .. } => content
                .iter_mut()
                .filter_map(|block| match block {
                    ModelToolReturnPart::Attachment(source) => Some(source),
                    ModelToolReturnPart::Text { .. } | ModelToolReturnPart::Retained(_) => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Every output a tool result retained out of history (FIG-1643), in
    /// content order. The commit that writes the part roots each one's
    /// attachment.
    pub fn retained_outputs(&self) -> impl Iterator<Item = &crate::RetainedOutput> {
        self.tool_result_content()
            .unwrap_or_default()
            .iter()
            .filter_map(ModelToolReturnPart::retained)
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

    /// Lash's identity for the call this part issues or answers; `Some` for
    /// tool calls and tool results.
    pub fn call_id(&self) -> Option<&ToolCallId> {
        match self {
            Self::ToolCall { call_id, .. } | Self::ToolResult { call_id, .. } => Some(call_id),
            _ => None,
        }
    }

    /// The provider's correlation for a tool call; `Some` only for tool-call
    /// parts. A result carries none: it answers its call by
    /// [`Part::call_id`].
    pub fn provider_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolCall {
                provider_call_id, ..
            } => Some(provider_call_id),
            _ => None,
        }
    }

    /// The invoked tool's registered name; `Some` for the same parts as
    /// [`Part::call_id`].
    pub fn tool_name(&self) -> Option<&str> {
        match self {
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
        }
    }

    pub fn code(id: String, content: String) -> Self {
        Self::Code { id, content }
    }

    pub fn output(id: String, content: String) -> Self {
        Self::Output { id, content }
    }

    pub fn error(id: String, content: String) -> Self {
        Self::Error { id, content }
    }

    pub fn prose(id: String, content: String, response_meta: Option<ResponseTextMeta>) -> Self {
        Self::Prose {
            id,
            content,
            response_meta,
        }
    }

    pub fn tool_call(
        id: String,
        content: String,
        call_id: ToolCallId,
        provider_call_id: String,
        tool_name: String,
        tool_replay: Option<ProviderReplayMeta>,
    ) -> Self {
        Self::ToolCall {
            id,
            content,
            call_id,
            provider_call_id,
            tool_name,
            tool_replay,
        }
    }

    /// The one result answering `call_id`, carrying the tool's text and
    /// attachment blocks in order.
    pub fn tool_result(
        id: String,
        content: Vec<ModelToolReturnPart>,
        call_id: ToolCallId,
        tool_name: String,
        status: crate::ToolCallStatus,
    ) -> Self {
        Self::ToolResult {
            status,
            id,
            content,
            call_id,
            tool_name,
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

fn attachment_from_part(part: &Part) -> Option<AttachmentRef> {
    if !matches!(part.kind(), PartKind::Attachment) {
        return None;
    }
    let attachment = part.attachment()?;
    Some(attachment.reference.clone())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub messages: Vec<LlmMessage>,
}

impl RenderedPrompt {
    /// Refs in message order, derived from the structured blocks,
    /// including the attachments inside tool results.
    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        self.messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .flat_map(LlmContentBlock::attachments)
    }
}

/// Mode-owned history text. Core supplies each record in source order and
/// retains structured tool calls, results, reasoning and attachment references.
/// A policy must be pure; capture its complete immutable parameters at creation.
#[derive(Clone)]
pub struct HistoryPresentation(Arc<HistoryPartRenderer>);

type HistoryPartRenderer = dyn Fn(MessageRole, &Part) -> String + Send + Sync;

impl std::fmt::Debug for HistoryPresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryPresentation")
            .finish_non_exhaustive()
    }
}

impl Default for HistoryPresentation {
    fn default() -> Self {
        Self::verbatim()
    }
}

impl HistoryPresentation {
    pub fn new(render: impl Fn(MessageRole, &Part) -> String + Send + Sync + 'static) -> Self {
        Self(Arc::new(render))
    }

    /// Structural projection for drivers that send record text verbatim.
    pub fn verbatim() -> Self {
        static POLICY: OnceLock<HistoryPresentation> = OnceLock::new();
        POLICY
            .get_or_init(|| Self::new(|_, part| part.content().into_owned()))
            .clone()
    }

    pub fn render(&self, messages: &[Message]) -> RenderedPrompt {
        let mut rendered = RenderedPrompt::default();
        append_structured_prompt_tail(messages, &[], self, |message| {
            rendered.messages.push(message)
        });
        rendered
    }
}

fn tool_result_visible_text(content: &[ModelToolReturnPart]) -> Cow<'_, str> {
    match content {
        [] => Cow::Borrowed(""),
        [part] => Cow::Borrowed(part.visible_text().unwrap_or_default()),
        _ => Cow::Owned(
            content
                .iter()
                .filter_map(ModelToolReturnPart::visible_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

/// Memoized render of a `MessageSequence`'s `base`. Shared across the
/// per-iteration `MessageSequence` instances that wrap the same base
/// (typically the `SessionGraphCache`'s projected messages) so the
/// chat projector's `render_prompt` walk happens once per turn instead
/// of once per LLM iteration.
///
/// A cache made for a base that extends an earlier one
/// ([`Self::extending`]) renders only the appended messages onto the earlier
/// cache's render, sharing its buffer: a turn renders its own messages, not
/// the frame, and every retained read model shares one rendered prefix
/// (FIG-4059).
#[derive(Debug, Default)]
pub struct BaseRenderCache {
    rendered: std::sync::Mutex<
        Option<(
            HistoryPresentation,
            usize,
            crate::llm::types::RequestMessages,
        )>,
    >,
    /// The cache of the base this one's base extends, with that base's
    /// length. Taken by the first render, so a chain of caches never
    /// outlives it.
    extends: std::sync::Mutex<Option<(Arc<BaseRenderCache>, usize)>>,
}

impl BaseRenderCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A cache for a base whose first `prefix_len` messages are the base
    /// `previous` renders.
    #[must_use]
    pub fn extending(previous: Arc<BaseRenderCache>, prefix_len: usize) -> Self {
        Self {
            rendered: std::sync::Mutex::new(None),
            extends: std::sync::Mutex::new(Some((previous, prefix_len))),
        }
    }

    /// The render of `base`, the messages this cache was made for.
    /// Longest recorded projection prefix matching a request, across policy identities.
    pub fn shared_prefix(&self, rendered: &[LlmMessage]) -> Option<usize> {
        self.rendered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(_, _, cached)| {
                cached
                    .iter()
                    .zip(rendered)
                    .take_while(|(a, b)| a == b)
                    .count()
            })
            .max()
    }

    pub fn shared_retained_prefix(
        &self,
        rendered: &crate::llm::types::RequestMessages,
    ) -> Option<usize> {
        self.rendered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(_, _, cached)| {
                let shared = cached.shared_prefix(rendered);
                // A reconstructed window may have equal values in different
                // allocations. Reconcile only the suffix whose identity is
                // not already witnessed by the persistent representation.
                shared
                    + cached
                        .iter_from(shared)
                        .zip(rendered.iter_from(shared))
                        .take_while(|(before, now)| before == now)
                        .count()
            })
            .max()
    }

    #[expect(
        clippy::expect_used,
        reason = "the public flat adapter uses an unowned budget which cannot refuse"
    )]
    pub fn rendered(
        &self,
        base: &[Message],
        policy: &HistoryPresentation,
    ) -> AppendVec<LlmMessage> {
        AppendVec::from(
            self.rendered_retained(
                base,
                policy,
                &crate::allocation::AllocationBudget::unowned(),
            )
            .expect("independent flat render")
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        )
    }

    pub fn rendered_retained(
        &self,
        base: &[Message],
        policy: &HistoryPresentation,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<crate::llm::types::RequestMessages, crate::allocation::AllocationError> {
        let mut cached = self.rendered.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, _, rendered)) = cached.iter().find(|(previous, owner, _)| {
            Arc::ptr_eq(&previous.0, &policy.0) && *owner == budget.identity()
        }) {
            return Ok(rendered.clone());
        }
        let prefix = self
            .extends
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .and_then(|(previous, prefix_len)| {
                let cached = previous.rendered.lock().unwrap_or_else(|p| p.into_inner());
                let (_, _, rendered) = cached.iter().find(|(previous, owner, _)| {
                    Arc::ptr_eq(&previous.0, &policy.0) && *owner == budget.identity()
                })?;
                (prefix_len <= base.len()).then(|| (rendered.clone(), prefix_len))
            });
        let (rendered, start) = prefix.unwrap_or_default();
        let mut tail = Vec::new();
        append_structured_prompt_tail_owned(
            &base[start..],
            &base[..start],
            policy,
            budget,
            |message| tail.push(message),
        )?;
        let rendered = rendered.append_rendered(&tail, budget)?;
        *cached = Some((policy.clone(), budget.identity(), rendered.clone()));
        self.extends
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        Ok(rendered)
    }
}

mod sequence;
pub use sequence::{MessageSequence, MessageSequenceIter};

pub fn render_prompt(msgs: &[Message]) -> RenderedPrompt {
    let mut rendered = RenderedPrompt::default();
    append_rendered_prompt(&mut rendered, msgs);
    rendered
}

pub fn messages_are_prompt_resume_safe<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> bool {
    let mut count = 0;
    let safe = prompt_resume_safe_len(messages.into_iter().inspect(|_| count += 1));
    safe == count
}

/// How many of `messages` lead a prompt-resume-safe prefix, the longest
/// one: every tool call in it is answered, and it pairs results with calls
/// as [`messages_are_prompt_resume_safe`] requires.
pub fn prompt_resume_safe_len<'a>(messages: impl IntoIterator<Item = &'a Message>) -> usize {
    let mut seen_tool_calls = HashSet::new();
    let mut completed_tool_calls = HashSet::new();
    let mut safe = 0;

    for (index, message) in messages.into_iter().enumerate() {
        // A cell's assistant context is answered by its cell record, the
        // protocol event committed with it, so its call awaits no tool
        // result.
        let cell_context = matches!(
            message.origin,
            Some(MessageOrigin::TurnOutput {
                cell_id: Some(_),
                ..
            })
        );
        // Reasoning parts don't participate in tool pairing and are always
        // safe to resume through.
        for part in message.parts.iter().filter(|_| !cell_context) {
            match part.kind() {
                PartKind::ToolCall => {
                    if !matches!(message.role, MessageRole::Assistant) {
                        return safe;
                    }
                    let Some(call_id) = part.call_id() else {
                        return safe;
                    };
                    if !seen_tool_calls.insert(call_id) {
                        return safe;
                    }
                }
                PartKind::ToolResult => {
                    if !matches!(message.role, MessageRole::User) {
                        return safe;
                    }
                    let Some(call_id) = part.call_id() else {
                        return safe;
                    };
                    if !seen_tool_calls.contains(call_id) {
                        return safe;
                    }
                    if !completed_tool_calls.insert(call_id) {
                        return safe;
                    }
                }
                _ => {}
            }
        }
        if seen_tool_calls.len() == completed_tool_calls.len() {
            safe = index + 1;
        }
    }
    safe
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
    append_structured_prompt_tail(msgs, &[], &HistoryPresentation::verbatim(), |message| {
        rendered.messages.push(message)
    });
}

fn append_structured_prompt_tail(
    appended: &[Message],
    prefix: &[Message],
    policy: &HistoryPresentation,
    mut push: impl FnMut(LlmMessage),
) {
    let provider_call_ids = appended
        .iter()
        .flat_map(|msg| msg.parts.iter())
        .filter_map(|part| Some((part.call_id()?, part.provider_call_id()?)))
        .collect::<std::collections::HashMap<_, _>>();
    for msg in appended {
        if let Some(projected) = render_structured_message(msg, policy, |call_id| {
            provider_call_ids
                .get(call_id)
                .copied()
                .or_else(|| {
                    prefix.iter().rev().find_map(|earlier| {
                        earlier.parts.iter().rev().find_map(|part| {
                            (part.call_id() == Some(call_id))
                                .then(|| part.provider_call_id())
                                .flatten()
                        })
                    })
                })
                .map(str::to_string)
        }) {
            push(projected);
        }
    }
}

/// One message's render, `None` when it has no block to send.
/// `provider_call_id` answers the provider correlation of the call a tool
/// result pairs with.
fn render_structured_message(
    msg: &Message,
    policy: &HistoryPresentation,
    provider_call_id: impl Fn(&ToolCallId) -> Option<String>,
) -> Option<LlmMessage> {
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
                let call_id = part.provider_call_id().unwrap_or_default().to_string();
                let tool_name = part.tool_name().unwrap_or_default().to_string();
                blocks.push(LlmContentBlock::ToolCall {
                    call_id,
                    tool_name,
                    input_json: part.content().to_string(),
                    replay: part.tool_replay().cloned(),
                });
            }
            PartKind::ToolResult => {
                // A result whose call the window no longer holds has no
                // provider correlation to answer with; it is not
                // resume-safe (`messages_are_prompt_resume_safe`) and is
                // left out rather than answered under an invented id.
                let Some(call_id) = part.call_id().and_then(&provider_call_id) else {
                    continue;
                };
                let Part::ToolResult { status, .. } = part else {
                    continue;
                };
                blocks.push(LlmContentBlock::ToolResult {
                    status: *status,
                    call_id,
                    content: part.tool_result_content().unwrap_or_default().to_vec(),
                    tool_name: part.tool_name().map(str::to_string),
                });
            }
            _ => {
                if let Some(attachment) = attachment_from_part(part)
                    && matches!(msg.role, MessageRole::User)
                {
                    blocks.push(LlmContentBlock::Attachment {
                        reference: Box::new(attachment),
                    });
                    continue;
                }

                let text = (policy.0)(msg.role, part);
                if text.trim().is_empty() {
                    continue;
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
        return None;
    }
    let mut projected = LlmMessage::new(llm_role_for_message(msg.role), blocks);
    projected.starts_user_segment = matches!(
        (msg.role, msg.origin.as_ref()),
        (MessageRole::User, Some(MessageOrigin::TurnInput { .. }))
    );
    Some(projected)
}

fn append_structured_prompt_tail_owned(
    appended: &[Message],
    prefix: &[Message],
    policy: &HistoryPresentation,
    budget: &crate::allocation::AllocationBudget,
    mut push: impl FnMut(crate::llm::types::AdmittedMessage),
) -> Result<(), crate::allocation::AllocationError> {
    let provider_call_ids = appended
        .iter()
        .flat_map(|msg| msg.parts.iter())
        .filter_map(|part| Some((part.call_id()?, part.provider_call_id()?)))
        .collect::<std::collections::HashMap<_, _>>();
    for msg in appended {
        if let Some(projected) = render_structured_message_owned(msg, policy, budget, |call_id| {
            provider_call_ids
                .get(call_id)
                .copied()
                .or_else(|| {
                    prefix.iter().rev().find_map(|earlier| {
                        earlier.parts.iter().rev().find_map(|part| {
                            (part.call_id() == Some(call_id))
                                .then(|| part.provider_call_id())
                                .flatten()
                        })
                    })
                })
                .map(str::to_string)
        })? {
            push(projected);
        }
    }
    Ok(())
}

/// One message's render, `None` when it has no block to send.
/// `provider_call_id` answers the provider correlation of the call a tool
/// result pairs with.
fn render_structured_message_owned(
    msg: &Message,
    policy: &HistoryPresentation,
    budget: &crate::allocation::AllocationBudget,
    provider_call_id: impl Fn(&ToolCallId) -> Option<String>,
) -> Result<Option<crate::llm::types::AdmittedMessage>, crate::allocation::AllocationError> {
    let hold = budget.reserve(crate::allocation::payload_capacity(
        msg,
        msg.parts.len()
            * (std::mem::size_of::<LlmContentBlock>()
                + std::mem::size_of::<crate::allocation::AllocationHold>()),
    ))?;
    let mut holds = Vec::with_capacity(msg.parts.len() + 1);
    holds.push(hold);
    let mut blocks: Vec<LlmContentBlock> = Vec::with_capacity(msg.parts.len());
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
                let call_id = part.provider_call_id().unwrap_or_default().to_string();
                let tool_name = part.tool_name().unwrap_or_default().to_string();
                blocks.push(LlmContentBlock::ToolCall {
                    call_id,
                    tool_name,
                    input_json: part.content().to_string(),
                    replay: part.tool_replay().cloned(),
                });
            }
            PartKind::ToolResult => {
                // A result whose call the window no longer holds has no
                // provider correlation to answer with; it is not
                // resume-safe (`messages_are_prompt_resume_safe`) and is
                // left out rather than answered under an invented id.
                let Some(call_id) = part.call_id().and_then(&provider_call_id) else {
                    continue;
                };
                let Part::ToolResult { status, .. } = part else {
                    continue;
                };
                blocks.push(LlmContentBlock::ToolResult {
                    status: *status,
                    call_id,
                    content: part.tool_result_content().unwrap_or_default().to_vec(),
                    tool_name: part.tool_name().map(str::to_string),
                });
            }
            _ => {
                if let Some(attachment) = attachment_from_part(part)
                    && matches!(msg.role, MessageRole::User)
                {
                    blocks.push(LlmContentBlock::Attachment {
                        reference: Box::new(attachment),
                    });
                    continue;
                }

                // Host callbacks own their temporary output. Reserve its
                // returned capacity before this owner retains the string.
                let text = (policy.0)(msg.role, part);
                holds
                    .push(budget.reserve(
                        (text.capacity() + std::mem::size_of::<Arc<str>>() + 16) as u64,
                    )?);
                if text.trim().is_empty() {
                    continue;
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
        return Ok(None);
    }
    let mut projected = LlmMessage::new(llm_role_for_message(msg.role), blocks);
    projected.starts_user_segment = matches!(
        (msg.role, msg.origin.as_ref()),
        (MessageRole::User, Some(MessageOrigin::TurnInput { .. }))
    );
    Ok(Some(crate::llm::types::AdmittedMessage {
        message: projected,
        _holds: holds.into(),
    }))
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
