//! Persistent owner requests and their compatible message digest.
use super::*;

/// Immutable request messages shared across owner drafts and held readers.
#[derive(Clone, Debug, Default)]
pub struct RequestMessages(
    crate::persistent_sequence::PersistentSequence<AdmittedMessage>,
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
#[derive(Clone, Debug)]
pub(crate) struct AdmittedMessage {
    pub(crate) message: LlmMessage,
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
    _hold: crate::allocation::AllocationHold,
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
        reason = "the digest writer is infallible and each checkpoint consumes its pre-reserved state"
    )]
    fn append<'a>(
        &self,
        mut len: usize,
        tail: impl Iterator<Item = &'a LlmMessage>,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
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
        let mut hold = None;
        for message in tail {
            if hold.is_none() {
                hold = Some(budget.reserve((std::mem::size_of::<MessageDigest>() + 16) as u64)?);
            }
            if len > 0 {
                hasher.update(b",");
            }
            serde_json::to_writer(Writer(&mut hasher), message)
                .expect("model messages contain JSON primitives");
            len += 1;
            if len.is_multiple_of(32) {
                let state = Arc::new(MessageDigest {
                    hasher: hasher.clone(),
                    _hold: hold.take().expect("admitted digest state"),
                });
                next.checkpoints = next
                    .checkpoints
                    .append(std::slice::from_ref(&state), budget)?;
                next.state = Some(state);
            }
        }
        if let Some(hold) = hold {
            next.state = Some(Arc::new(MessageDigest {
                hasher,
                _hold: hold,
            }));
        }
        Ok(next)
    }
    fn prefix(
        &self,
        len: usize,
        messages: &RequestMessages,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let count = len / 32;
        let state = count
            .checked_sub(1)
            .and_then(|index| self.checkpoints.get(index))
            .cloned();
        Self {
            state,
            checkpoints: self.checkpoints.prefix(count, budget)?,
        }
        .append(
            count * 32,
            messages.iter_from(count * 32).take(len - count * 32),
            budget,
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
        Self::from_messages(messages, &crate::allocation::AllocationBudget::unowned())
            .map_err(serde::de::Error::custom)
    }
}

impl RequestMessages {
    /// The compatible v1 journal digest, finalized without visiting history.
    pub fn content_digest(&self) -> String {
        self.2.finish()
    }
    pub fn from_messages(
        messages: Vec<LlmMessage>,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        Self::default().append(&messages, budget)
    }
    pub fn append(
        &self,
        tail: &[LlmMessage],
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        if tail.is_empty() {
            return Ok(self.clone());
        }
        let _scratch = budget.reserve(
            (tail.len() as u64).saturating_mul(std::mem::size_of::<AdmittedMessage>() as u64 + 32),
        )?;
        let totals = MessageTotals::of(tail.iter());
        let admitted = tail
            .iter()
            .map(|message| {
                let blocks = LlmMessageBlocks::adopt_in(&message.blocks, budget)?;
                let mut message = message.clone();
                message.blocks = blocks;
                Ok(AdmittedMessage { message })
            })
            .collect::<Result<Vec<_>, crate::allocation::AllocationError>>()?;
        Ok(Self(
            self.0.append(&admitted, budget)?,
            MessageTotals {
                attachments: self.1.attachments + totals.attachments,
                replay: self.1.replay + totals.replay,
                chars: self.1.chars + totals.chars,
                dependent: self.1.dependent + totals.dependent,
            },
            self.2.append(self.len(), tail.iter(), budget)?,
        ))
    }
    pub(crate) fn append_rendered(
        &self,
        tail: &[AdmittedMessage],
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let totals = MessageTotals::of(tail.iter().map(|entry| &entry.message));
        Ok(Self(
            self.0.append(tail, budget)?,
            MessageTotals {
                attachments: self.1.attachments + totals.attachments,
                replay: self.1.replay + totals.replay,
                chars: self.1.chars + totals.chars,
                dependent: self.1.dependent + totals.dependent,
            },
            self.2
                .append(self.len(), tail.iter().map(|entry| &entry.message), budget)?,
        ))
    }
    pub fn prefix(
        &self,
        len: usize,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        if len == self.len() {
            return Ok(self.clone());
        }
        let removed = MessageTotals::of(self.iter_from(len));
        Ok(Self(
            self.0.prefix(len, budget)?,
            MessageTotals {
                attachments: self.1.attachments - removed.attachments,
                replay: self.1.replay - removed.replay,
                chars: self.1.chars - removed.chars,
                dependent: self.1.dependent - removed.dependent,
            },
            self.2.prefix(len, self, budget)?,
        ))
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
        self.0.iter().map(|entry| &entry.message)
    }
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &LlmMessage> {
        self.0.iter_from(start).map(|entry| &entry.message)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn shared_prefix(&self, other: &Self) -> usize {
        self.0.shared_prefix(&other.0, |a, b| {
            let (a, b) = (&a.message, &b.message);
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
    envelope: Arc<AdmittedEnvelope>,
    pub messages: RequestMessages,
}

#[derive(Debug)]
struct AdmittedEnvelope {
    request: LlmRequest,
    _hold: crate::allocation::AllocationHold,
}

impl RetainedLlmRequest {
    pub fn adopt_in(
        &self,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let hold = budget.reserve(crate::allocation::payload_capacity(
            self.envelope(),
            std::mem::size_of::<AdmittedEnvelope>() + 256,
        ))?;
        let mut messages = RequestMessages::default();
        for message in self.messages.iter() {
            messages = messages.append(std::slice::from_ref(message), budget)?;
        }
        let tools = LlmToolSpecs::adopt_tools_in(&self.tools, budget)?;
        let mut request = self.envelope().clone();
        request.tools = tools;
        Ok(Self::admitted_envelope(request, messages, hold))
    }

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

    #[expect(
        clippy::expect_used,
        reason = "the explicit cold adapter uses an unowned budget which cannot refuse"
    )]
    pub fn from_request(mut request: LlmRequest) -> Self {
        let messages = RequestMessages::from_messages(
            std::mem::take(&mut request.messages),
            &crate::allocation::AllocationBudget::unowned(),
        )
        .expect("independent request construction has no actor budget");
        Self {
            envelope: Arc::new(AdmittedEnvelope {
                request,
                _hold: Default::default(),
            }),
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
        let mut request = self.envelope.request.clone();
        request.messages = self.messages.iter().cloned().collect();
        request
    }
    pub fn envelope(&self) -> &LlmRequest {
        &self.envelope.request
    }
    /// The producer reserved `hold` before building the fresh envelope.
    pub fn admitted_envelope(
        request: LlmRequest,
        messages: RequestMessages,
        hold: crate::allocation::AllocationHold,
    ) -> Self {
        debug_assert!(request.messages.is_empty());
        Self {
            envelope: Arc::new(AdmittedEnvelope {
                request,
                _hold: hold,
            }),
            messages,
        }
    }
    /// A host-produced flat value is charged before this owner retains it.
    pub fn try_from_request(
        mut request: LlmRequest,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let hold = budget.reserve(crate::allocation::payload_capacity(
            &request,
            std::mem::size_of::<AdmittedEnvelope>(),
        ))?;
        let messages =
            RequestMessages::from_messages(std::mem::take(&mut request.messages), budget)?;
        request.tools = LlmToolSpecs::adopt_tools_in(&request.tools, budget)?;
        Ok(Self::admitted_envelope(request, messages, hold))
    }
    /// Admit a borrowed callback result before cloning any owned envelope fields.
    pub fn try_from_flat(
        request: &LlmRequest,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let hold = budget.reserve(crate::allocation::payload_capacity(
            request,
            std::mem::size_of::<AdmittedEnvelope>(),
        ))?;
        let messages = RequestMessages::default().append(&request.messages, budget)?;
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
            stream_events,
            provider_trace,
            attempt_control,
        } = request;
        let envelope = LlmRequest {
            instructions: instructions.clone(),
            model: model.clone(),
            messages: Vec::new(),
            tools: LlmToolSpecs::adopt_tools_in(tools, budget)?,
            tool_choice: tool_choice.clone(),
            attachment_acceptance: attachment_acceptance.clone(),
            generation: generation.clone(),
            scope: scope.clone(),
            output_spec: output_spec.clone(),
            stream_events: stream_events.clone(),
            provider_trace: provider_trace.clone(),
            attempt_control: attempt_control.clone(),
        };
        Ok(Self::admitted_envelope(envelope, messages, hold))
    }
    pub fn edit_envelope(
        &mut self,
        budget: &crate::allocation::AllocationBudget,
        additional: u64,
        edit: impl FnOnce(&mut LlmRequest),
    ) -> Result<(), crate::allocation::AllocationError> {
        let bound = crate::allocation::payload_capacity(
            self.envelope(),
            std::mem::size_of::<AdmittedEnvelope>(),
        )
        .saturating_add(additional);
        let hold = budget.reserve(bound)?;
        let mut request = self.envelope.request.clone();
        edit(&mut request);
        self.envelope = Arc::new(AdmittedEnvelope {
            request,
            _hold: hold,
        });
        Ok(())
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
        self.0.get(index).map(|entry| &entry.message)
    }
    pub fn first(&self) -> Option<&LlmMessage> {
        self.get(0)
    }
    pub fn last(&self) -> Option<&LlmMessage> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }
    pub fn changed_allocations(&self, previous: &Self, visit: impl FnMut(usize, u64)) {
        self.0.changed_allocations(&previous.0, visit);
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
