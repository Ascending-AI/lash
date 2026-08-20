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
        Self {
            id: message.id.as_str(),
            role: message.role,
            parts: &message.parts,
            origin: message.origin.as_ref(),
        }
    }
}

impl<'a> From<&'a super::ConversationRecord> for MessageContentRef<'a> {
    fn from(record: &'a super::ConversationRecord) -> Self {
        Self {
            id: record.id.as_str(),
            role: record.role,
            parts: &record.parts,
            origin: record.origin.as_ref(),
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
pub enum MessageOrigin {
    Plugin {
        plugin_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
    },
    Process {
        process_id: String,
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
        turn_id: String,
        /// The durable turn input this message was materialized from, present
        /// when the input arrived through queued ingress and absent when the
        /// turn was driven with its input in hand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct Part {
    /// e.g. "m3.p0"
    pub id: String,
    pub kind: PartKind,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<PartAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Opaque provider replay state attached to a `ToolCall` part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_replay: Option<ProviderReplayMeta>,
    pub prune_state: PruneState,
    /// Populated only for `PartKind::Reasoning` parts. Carries opaque
    /// provider replay metadata so the adapter can re-emit the exact same
    /// reasoning item on subsequent turns.
    /// `#[serde(default, skip_serializing_if)]` so older snapshots that
    /// predate this field round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_meta: Option<ProviderReasoningReplay>,
    /// Provider message metadata for assistant text parts. Legacy snapshots
    /// omit it; adapters synthesize deterministic ids when replaying older
    /// assistant text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_meta: Option<ResponseTextMeta>,
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
    fn base(id: String, kind: PartKind, content: String) -> Self {
        Self {
            id,
            kind,
            content,
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }
    }

    pub fn text(id: String, content: String, response_meta: Option<ResponseTextMeta>) -> Self {
        Self {
            response_meta,
            ..Self::base(id, PartKind::Text, content)
        }
    }

    pub fn attachment_part(
        id: String,
        content: String,
        attachment: Option<PartAttachment>,
    ) -> Self {
        Self {
            attachment,
            ..Self::base(id, PartKind::Attachment, content)
        }
    }

    pub fn tool_result_attachment(
        id: String,
        content: String,
        attachment: PartAttachment,
        tool_call_id: String,
        tool_name: String,
    ) -> Self {
        Self {
            attachment: Some(attachment),
            tool_call_id: Some(tool_call_id),
            tool_name: Some(tool_name),
            ..Self::base(id, PartKind::Attachment, content)
        }
    }

    pub fn code(id: String, content: String) -> Self {
        Self::base(id, PartKind::Code, content)
    }

    pub fn output(id: String, content: String) -> Self {
        Self::base(id, PartKind::Output, content)
    }

    pub fn error(id: String, content: String) -> Self {
        Self::base(id, PartKind::Error, content)
    }

    pub fn prose(id: String, content: String, response_meta: Option<ResponseTextMeta>) -> Self {
        Self {
            response_meta,
            ..Self::base(id, PartKind::Prose, content)
        }
    }

    pub fn tool_call(
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
        tool_replay: Option<ProviderReplayMeta>,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id),
            tool_name: Some(tool_name),
            tool_replay,
            ..Self::base(id, PartKind::ToolCall, content)
        }
    }

    pub fn tool_result(
        id: String,
        content: String,
        tool_call_id: String,
        tool_name: String,
    ) -> Self {
        Self {
            tool_call_id: Some(tool_call_id),
            tool_name: Some(tool_name),
            ..Self::base(id, PartKind::ToolResult, content)
        }
    }

    pub fn reasoning(
        id: String,
        content: String,
        reasoning_meta: Option<ProviderReasoningReplay>,
    ) -> Self {
        Self {
            reasoning_meta,
            ..Self::base(id, PartKind::Reasoning, content)
        }
    }

    #[cfg(test)]
    pub(crate) fn prompt_char_count(&self) -> usize {
        // Reasoning parts are not user-visible text and aren't sent to the
        // model as flat prompt content. Provider adapters may re-emit them
        // via structured replay metadata instead. Excluding them from the
        // accounting keeps the rolling-history plugin's prune decisions
        // driven by real conversation content.
        if matches!(self.kind, PartKind::Reasoning) {
            return 0;
        }
        if matches!(self.kind, PartKind::Attachment) {
            return self
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.source.stored_ref())
                .map(|attachment_ref| attachment_ref.id.as_str().len())
                .unwrap_or_else(|| self.render().len());
        }
        self.render().len()
    }

    pub(crate) fn render(&self) -> String {
        if matches!(self.kind, PartKind::Attachment) {
            return if self.attachment.is_some() || self.content.trim().is_empty() {
                "[Attachment]".to_string()
            } else {
                self.content.clone()
            };
        }
        match &self.prune_state {
            PruneState::Intact => self.content.clone(),
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
        MessageRole::System => match part.kind {
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
        MessageRole::Assistant => match part.kind {
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
    let tool_name = part.tool_name.as_deref().unwrap_or("tool");
    let trimmed = rendered.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        format!("{tool_name}()")
    } else {
        format!("{tool_name}({trimmed})")
    }
}

fn attachment_from_part(part: &Part) -> Option<AttachmentSource> {
    if !matches!(part.kind, PartKind::Attachment) {
        return None;
    }
    let attachment = part.attachment.as_ref()?;
    Some(attachment.source.clone())
}

fn render_message_for_transcript(msg: &Message, attachments: &mut Vec<AttachmentSource>) -> String {
    let mut out = Vec::new();
    for part in msg.parts.iter() {
        // Reasoning items are display-only from the transcript's point of
        // view — they are never replayed as flat text. Provider adapters use
        // structured replay metadata when they can re-emit reasoning.
        if matches!(part.kind, PartKind::Reasoning) {
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
    pub attachments: Vec<AttachmentSource>,
}

/// Memoized render of a `MessageSequence`'s `base`. Shared across the
/// per-iteration `MessageSequence` instances that wrap the same base
/// (typically the `SessionGraphCache`'s projected messages) so the
/// chat projector's `render_prompt` walk happens once per turn instead
/// of once per LLM iteration.
pub type BaseRenderCache = OnceLock<RenderedPrompt>;

#[derive(Debug)]
pub struct MessageSequence {
    base: Arc<Vec<Message>>,
    delta: Vec<Message>,
    owned: Option<Vec<Message>>,
    materialized: OnceLock<Arc<Vec<Message>>>,
    base_rendered: Option<Arc<BaseRenderCache>>,
}

impl Clone for MessageSequence {
    fn clone(&self) -> Self {
        Self {
            base: Arc::clone(&self.base),
            delta: self.delta.clone(),
            owned: self.owned.clone(),
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
            base: Arc::new(Vec::new()),
            delta: Vec::new(),
            owned: Some(messages),
            materialized: OnceLock::new(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base(base: Arc<Vec<Message>>) -> Self {
        Self {
            base,
            delta: Vec::new(),
            owned: None,
            materialized: OnceLock::new(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base_and_delta(base: Arc<Vec<Message>>, delta: Vec<Message>) -> Self {
        Self {
            base,
            delta,
            owned: None,
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
        match &self.owned {
            Some(owned) => owned.len(),
            None => self.base.len() + self.delta.len(),
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
        if self.owned.is_some() || next.owned.is_some() {
            return None;
        }
        if !Arc::ptr_eq(&self.base, &next.base) {
            return None;
        }
        let tail = next.delta.get(self.delta.len()..)?;
        self.delta
            .iter()
            .zip(next.delta.iter())
            .all(|(current, candidate)| message_content_equal(current, candidate))
            .then_some(tail)
    }

    pub(crate) fn iter(&self) -> MessageSequenceIter<'_> {
        match self.owned.as_ref() {
            Some(owned) => MessageSequenceIter::Owned(owned.iter()),
            None => MessageSequenceIter::Split(self.base.iter().chain(self.delta.iter())),
        }
    }

    pub(crate) fn as_slice(&self) -> &[Message] {
        if let Some(owned) = &self.owned {
            return owned.as_slice();
        }
        if self.delta.is_empty() {
            return self.base.as_slice();
        }
        self.materialized
            .get_or_init(|| {
                let mut combined = Vec::with_capacity(self.base.len() + self.delta.len());
                combined.extend(self.base.iter().cloned());
                combined.extend(self.delta.iter().cloned());
                Arc::new(combined)
            })
            .as_slice()
    }

    pub(crate) fn shared(&self) -> Arc<Vec<Message>> {
        if let Some(owned) = &self.owned {
            return Arc::clone(self.materialized.get_or_init(|| Arc::new(owned.clone())));
        }
        if self.delta.is_empty() {
            return Arc::clone(&self.base);
        }
        Arc::clone(self.materialized.get_or_init(|| {
            let mut combined = Vec::with_capacity(self.base.len() + self.delta.len());
            combined.extend(self.base.iter().cloned());
            combined.extend(self.delta.iter().cloned());
            Arc::new(combined)
        }))
    }

    pub fn make_mut(&mut self) -> &mut Vec<Message> {
        if self.owned.is_none() {
            let owned = if self.delta.is_empty() {
                Arc::unwrap_or_clone(Arc::clone(&self.base))
            } else if let Some(materialized) = self.materialized.get() {
                Arc::unwrap_or_clone(Arc::clone(materialized))
            } else {
                let mut combined = Vec::with_capacity(self.base.len() + self.delta.len());
                combined.extend(self.base.iter().cloned());
                combined.extend(self.delta.iter().cloned());
                combined
            };
            self.owned = Some(owned);
            self.base = Arc::new(Vec::new());
            self.delta.clear();
        }
        self.materialized = OnceLock::new();
        self.owned.as_mut().expect("message sequence owned state")
    }

    pub(crate) fn push(&mut self, message: Message) {
        if let Some(owned) = self.owned.as_mut() {
            owned.push(message);
        } else {
            self.delta.push(message);
        }
        self.materialized = OnceLock::new();
    }

    pub(crate) fn extend(&mut self, messages: Vec<Message>) {
        if messages.is_empty() {
            return;
        }
        if let Some(owned) = self.owned.as_mut() {
            owned.extend(messages);
        } else {
            self.delta.extend(messages);
        }
        self.materialized = OnceLock::new();
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.base = Arc::new(Vec::new());
        self.delta.clear();
        self.owned = Some(messages);
        self.materialized = OnceLock::new();
    }

    pub(crate) fn render_prompt(&self) -> RenderedPrompt {
        if let Some(owned) = &self.owned {
            return render_prompt(owned.as_slice());
        }
        if self.base.is_empty() {
            return render_prompt(self.delta.as_slice());
        }
        let mut rendered = match &self.base_rendered {
            Some(cache) => cache
                .get_or_init(|| render_prompt(self.base.as_slice()))
                .clone(),
            None => render_prompt(self.base.as_slice()),
        };
        if !self.delta.is_empty() {
            append_rendered_prompt(&mut rendered, self.delta.as_slice());
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
            if matches!(part.kind, PartKind::Reasoning) {
                continue;
            }
            match part.kind {
                PartKind::ToolCall => {
                    if !matches!(message.role, MessageRole::Assistant) {
                        return false;
                    }
                    let Some(call_id) = part
                        .tool_call_id
                        .as_deref()
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
                        .tool_call_id
                        .as_deref()
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

    RenderedPrompt {
        messages: vec![LlmMessage::text(LlmRole::User, text)],
        attachments,
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
            match part.kind {
                PartKind::Reasoning => {
                    let Some(meta) = part.reasoning_meta.as_ref() else {
                        continue;
                    };
                    if meta.is_empty() {
                        continue;
                    }
                    blocks.push(LlmContentBlock::Reasoning {
                        text: part.content.clone(),
                        replay: Some(meta.clone()),
                    });
                }
                PartKind::ToolCall => {
                    let call_id = part.tool_call_id.clone().unwrap_or_default();
                    let tool_name = part.tool_name.clone().unwrap_or_default();
                    blocks.push(LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json: part.content.clone(),
                        replay: part.tool_replay.clone(),
                    });
                }
                PartKind::ToolResult => {
                    let text = part.render();
                    let call_id = part.tool_call_id.clone().unwrap_or_default();
                    blocks.push(LlmContentBlock::ToolResult {
                        call_id,
                        content: text,
                        tool_name: part.tool_name.clone(),
                    });
                }
                _ => {
                    if let Some(attachment) = attachment_from_part(part)
                        && matches!(msg.role, MessageRole::User)
                    {
                        let attachment_idx = rendered.attachments.len();
                        rendered.attachments.push(attachment);
                        blocks.push(LlmContentBlock::Attachment { attachment_idx });
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
                        response_meta: if matches!(part.kind, PartKind::Text | PartKind::Prose) {
                            part.response_meta.clone()
                        } else {
                            None
                        },
                        cache_breakpoint: false,
                    });
                }
            }
        }
        if blocks.is_empty() {
            continue;
        }
        rendered
            .messages
            .push(LlmMessage::new(llm_role_for_message(msg.role), blocks));
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
