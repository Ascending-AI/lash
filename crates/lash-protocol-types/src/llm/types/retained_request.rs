//! Persistent owner requests and their compatible message digest.
use super::*;

/// Immutable request messages shared across owner drafts and held readers.
#[derive(Clone, Debug, Default)]
pub struct RequestMessages(
    crate::persistent_sequence::PersistentSequence<LlmMessage>,
    MessageTotals,
    MessageDigests,
);

#[derive(Clone, Copy, Debug, Default)]
struct MessageTotals {
    attachments: usize,
    replay: usize,
    chars: u128,
    dependent: usize,
}
impl MessageTotals {
    fn of(messages: impl Iterator<Item = impl std::borrow::Borrow<LlmMessage>>) -> Self {
        let mut totals = Self::default();
        for message in messages {
            let message = message.borrow();
            totals.dependent += usize::from(
                message.role == LlmRole::System
                    || (message.starts_user_segment
                        && (message.role != LlmRole::User
                            || message
                                .blocks
                                .iter()
                                .any(|block| matches!(block, LlmContentBlock::ToolResult { .. })))),
            );
            for block in message.blocks.iter() {
                totals.dependent += usize::from(match block {
                    LlmContentBlock::Text {
                        cache_breakpoint,
                        response_meta,
                        ..
                    } => *cache_breakpoint || response_meta.is_some(),
                    LlmContentBlock::Reasoning { .. } => true,
                    LlmContentBlock::ToolCall {
                        call_id, replay, ..
                    } => {
                        replay.is_some()
                            || call_id.is_empty()
                            || call_id.len() > 64
                            || !call_id.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                            })
                    }
                    LlmContentBlock::ToolResult { call_id, .. } => {
                        call_id.is_empty()
                            || call_id.len() > 64
                            || !call_id.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                            })
                    }
                    LlmContentBlock::Attachment { .. } => false,
                });
                totals.attachments += block.attachments().count();
                totals.replay += usize::from(match block {
                    LlmContentBlock::Text { response_meta, .. } => response_meta.is_some(),
                    LlmContentBlock::Reasoning { replay, .. } => replay.is_some(),
                    LlmContentBlock::ToolCall { replay, .. } => replay.is_some(),
                    _ => false,
                });
                totals.chars += (match block {
                    LlmContentBlock::Text { text, .. } => text.len(),
                    LlmContentBlock::Reasoning { text, .. } => text.len(),
                    LlmContentBlock::ToolCall { input_json, .. } => input_json.len(),
                    LlmContentBlock::Attachment { .. } => 256,
                    LlmContentBlock::ToolResult { content, .. } => content
                        .iter()
                        .map(|part| match part {
                            crate::ModelToolReturnPart::Text { text } => text.len(),
                            crate::ModelToolReturnPart::Retained(retained) => {
                                retained.witness.len()
                            }
                            crate::ModelToolReturnPart::Attachment(_) => 256,
                        })
                        .sum(),
                }) as u128;
                totals.chars += block
                    .attachments()
                    .map(|reference| u128::from(reference.byte_len / 4))
                    .sum::<u128>();
            }
        }
        totals
    }
}

// A checkpoint every leaf bounds rewrite hashing to the affected suffix plus
// at most 31 messages. States cover the array up to, but excluding, `]`.
#[derive(Clone, Debug, Default)]
struct MessageDigests {
    state: Option<Arc<MessageDigest>>,
    checkpoints: crate::persistent_sequence::PersistentSequence<Arc<MessageDigest>>,
}
#[derive(Debug)]
struct MessageDigest {
    hasher: crate::core_support::Blake3DomainHasher,
}
impl MessageDigests {
    fn initial() -> crate::core_support::Blake3DomainHasher {
        let mut hasher =
            crate::core_support::Blake3DomainHasher::new("lash-llm-request-content/v1");
        hasher.update(b"[");
        hasher
    }
    #[expect(
        clippy::expect_used,
        reason = "the digest writer accepts JSON primitives"
    )]
    fn append<'a>(&self, mut len: usize, tail: impl Iterator<Item = &'a LlmMessage>) -> Self {
        struct Writer<'a>(&'a mut crate::core_support::Blake3DomainHasher);
        impl std::io::Write for Writer<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut next = self.clone();
        let mut hasher = self
            .state
            .as_ref()
            .map_or_else(Self::initial, |state| state.hasher.clone());
        let mut changed = false;
        for message in tail {
            changed = true;
            if len > 0 {
                hasher.update(b",");
            }
            serde_json::to_writer(Writer(&mut hasher), message)
                .expect("model messages contain JSON primitives");
            len += 1;
            if len.is_multiple_of(32) {
                let state = Arc::new(MessageDigest {
                    hasher: hasher.clone(),
                });
                next.checkpoints = next.checkpoints.append(std::slice::from_ref(&state));
                next.state = Some(state);
                changed = false;
            }
        }
        if changed {
            next.state = Some(Arc::new(MessageDigest { hasher }));
        }
        next
    }
    fn prefix(&self, len: usize, messages: &RequestMessages) -> Self {
        let count = len / 32;
        let state = count
            .checked_sub(1)
            .and_then(|index| self.checkpoints.get(index))
            .cloned();
        Self {
            state,
            checkpoints: self.checkpoints.prefix(count),
        }
        .append(
            count * 32,
            messages.iter_from(count * 32).take(len - count * 32),
        )
    }
    fn finish(&self) -> String {
        let mut hasher = self
            .state
            .as_ref()
            .map_or_else(Self::initial, |state| state.hasher.clone());
        hasher.update(b"]");
        hasher.finalize_hex()
    }
}
impl serde::Serialize for RequestMessages {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}
impl<'de> serde::Deserialize<'de> for RequestMessages {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let messages = Vec::<LlmMessage>::deserialize(deserializer)?;
        Ok(Self::from_messages(messages))
    }
}

impl RequestMessages {
    /// The compatible v1 journal digest, finalized without visiting history.
    pub fn content_digest(&self) -> String {
        self.2.finish()
    }
    pub fn from_messages(messages: Vec<LlmMessage>) -> Self {
        Self::default().append(&messages)
    }
    pub fn append(&self, tail: &[LlmMessage]) -> Self {
        let totals = MessageTotals::of(tail.iter());
        Self(
            self.0.append(tail),
            MessageTotals {
                attachments: self.1.attachments + totals.attachments,
                replay: self.1.replay + totals.replay,
                chars: self.1.chars + totals.chars,
                dependent: self.1.dependent + totals.dependent,
            },
            self.2.append(self.len(), tail.iter()),
        )
    }
    pub fn prefix(&self, len: usize) -> Self {
        if len == self.len() {
            return self.clone();
        }
        let removed = MessageTotals::of(self.iter_from(len));
        Self(
            self.0.prefix(len),
            MessageTotals {
                attachments: self.1.attachments - removed.attachments,
                replay: self.1.replay - removed.replay,
                chars: self.1.chars - removed.chars,
                dependent: self.1.dependent - removed.dependent,
            },
            self.2.prefix(len, self),
        )
    }
    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        self.iter_from(if self.1.attachments == 0 {
            self.len()
        } else {
            0
        })
        .flat_map(|message| message.blocks.iter())
        .flat_map(LlmContentBlock::attachments)
    }
    pub fn iter(&self) -> impl Iterator<Item = &LlmMessage> {
        self.0.iter()
    }
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &LlmMessage> {
        self.0.iter_from(start)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn shared_prefix(&self, other: &Self) -> usize {
        self.0.shared_prefix(&other.0, |a, b| {
            a.role == b.role
                && a.starts_user_segment == b.starts_user_segment
                && Arc::ptr_eq(&a.blocks, &b.blocks)
        })
    }
}

/// A fresh request envelope and its immutable message tree. The public flat
/// request is materialized only at a consumer's explicit compatibility boundary.
#[derive(Clone, Debug)]
pub struct RetainedLlmRequest {
    envelope: Arc<LlmRequest>,
    pub messages: RequestMessages,
}

impl RetainedLlmRequest {
    pub fn estimated_tokens(&self) -> u32 {
        (((self.messages.1.chars + self.model.wire_model().len() as u128) / 4).max(1))
            .try_into()
            .unwrap_or(u32::MAX)
    }
    pub fn has_history_dependencies(&self) -> bool {
        self.messages.1.dependent != 0
            || matches!(
                self.model
                    .metadata()
                    .capability
                    .reasoning_retention
                    .selection,
                ReasoningRetentionSelection::ClientSideUserSegments { .. }
            )
    }
    pub fn has_replay(&self) -> bool {
        self.messages.1.replay > 0
    }
    pub fn has_attachments(&self) -> bool {
        self.messages.1.attachments > 0
    }

    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        self.messages.attachments()
    }

    pub fn from_request(mut request: LlmRequest) -> Self {
        let messages = RequestMessages::from_messages(std::mem::take(&mut request.messages));
        Self {
            envelope: Arc::new(request),
            messages,
        }
    }
    pub fn with_messages(&self, messages: RequestMessages) -> Self {
        Self {
            envelope: self.envelope.clone(),
            messages,
        }
    }
    pub fn messages(&self) -> &RequestMessages {
        &self.messages
    }
    pub fn materialize(&self) -> LlmRequest {
        let mut request = (*self.envelope).clone();
        request.messages = self.messages.iter().cloned().collect();
        request
    }
    pub fn envelope(&self) -> &LlmRequest {
        &self.envelope
    }
    pub fn from_envelope(request: LlmRequest, messages: RequestMessages) -> Self {
        debug_assert!(request.messages.is_empty());
        Self {
            envelope: Arc::new(request),
            messages,
        }
    }
    pub fn edit_envelope(&mut self, edit: impl FnOnce(&mut LlmRequest)) {
        edit(Arc::make_mut(&mut self.envelope));
    }
}

impl std::ops::Deref for RetainedLlmRequest {
    type Target = LlmRequest;
    fn deref(&self) -> &LlmRequest {
        self.envelope()
    }
}
impl RequestMessages {
    pub fn get(&self, index: usize) -> Option<&LlmMessage> {
        self.0.get(index)
    }
    pub fn first(&self) -> Option<&LlmMessage> {
        self.get(0)
    }
    pub fn last(&self) -> Option<&LlmMessage> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }
}

impl serde::Serialize for RetainedLlmRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let LlmRequest {
            instructions,
            model,
            messages: _,
            tools,
            tool_choice,
            attachment_acceptance,
            generation,
            scope,
            output_spec,
            stream_events: _,
            provider_trace: _,
            attempt_control: _,
        } = self.envelope();
        let mut state = serializer.serialize_struct(
            "LlmRequest",
            7 + usize::from(instructions.is_some())
                + usize::from(!attachment_acceptance.is_empty()),
        )?;
        if let Some(instructions) = instructions {
            state.serialize_field("instructions", instructions)?;
        }
        state.serialize_field("model", model)?;
        state.serialize_field("messages", &self.messages)?;
        state.serialize_field("tools", tools)?;
        state.serialize_field("tool_choice", tool_choice)?;
        if !attachment_acceptance.is_empty() {
            state.serialize_field("attachment_acceptance", attachment_acceptance)?;
        }
        state.serialize_field("generation", generation)?;
        state.serialize_field("scope", scope)?;
        state.serialize_field("output_spec", output_spec)?;
        state.end()
    }
}
impl<'de> serde::Deserialize<'de> for RetainedLlmRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        LlmRequest::deserialize(deserializer).map(Self::from_request)
    }
}
