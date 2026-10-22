//! Shared message sequences and their copy-on-write edits.

use super::*;

/// How a `MessageSequence` stores its messages.
///
/// `Layered` is the base/delta rope: a session-wide `base` shared as an
/// [`AppendVec`] plus the messages this iteration appended. `Owned` is a flat list the
/// sequence shares until mutation — the shape every sequence settles into after
/// `make_mut`/`replace`, and the only shape a deserialized sequence can
/// have. Layered-with-owned is not representable.
#[derive(Clone, Debug)]
enum SequenceMode {
    Layered {
        base: AppendVec<Message>,
        delta: Option<AppendVec<Message>>,
        joined: Option<AppendVec<Message>>,
    },
    Owned(AppendVec<Message>),
}

/// A clone shares its source buffers and prepared join. A fallible edit
/// publishes a complete new view; readers and effect bodies retain their
/// snapshots without copying the turn's history (FIG-5962).
#[derive(Clone, Debug)]
pub struct MessageSequence {
    mode: SequenceMode,
    base_rendered: Option<Arc<BaseRenderCache>>,
    // Raw mutable edits can introduce foreign parts or scalar allocations.
    admitted: bool,
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

// A `MessageSequence` is a shared base/delta rope; its meaningful
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
    pub fn from_owned(messages: Vec<Message>) -> Self {
        Self {
            mode: SequenceMode::Owned(AppendVec::from(messages)),
            base_rendered: None,
            admitted: false,
        }
    }

    pub fn from_base(base: AppendVec<Message>) -> Self {
        Self {
            mode: SequenceMode::Layered {
                base,
                delta: None,
                joined: None,
            },
            base_rendered: None,
            admitted: false,
        }
    }

    pub fn from_base_and_delta(
        base: AppendVec<Message>,
        delta: Vec<Message>,
    ) -> Result<Self, crate::allocation::AllocationError> {
        let mut sequence = Self::from_base(base);
        sequence.extend(delta)?;
        Ok(sequence)
    }

    /// Admit independently constructed source into this serving claim. A
    /// sequence over this ledger's immutable base keeps its sharing witness.
    pub fn clone_in(
        &self,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        fn buffer(
            source: &AppendVec<Message>,
            budget: &crate::allocation::AllocationBudget,
            admitted: bool,
        ) -> Result<AppendVec<Message>, crate::allocation::AllocationError> {
            use crate::allocation::SourceCapacity;
            if admitted && source.allocation_budget().identity() == budget.identity() {
                return Ok(source.clone());
            }
            let mut next = AppendVec::try_with_capacity(source.len(), budget)?;
            for message in source.iter() {
                let parts = MessageParts::adopt_in(&message.parts, budget)?;
                let _copy = budget.reserve(
                    message
                        .owned_capacity()
                        .saturating_add(std::mem::size_of::<Message>() as u64),
                )?;
                let mut copy = message.clone();
                copy.parts = parts;
                next.push(copy)?;
            }
            Ok(next)
        }
        match &self.mode {
            SequenceMode::Owned(owned) => Ok(Self {
                mode: SequenceMode::Owned(buffer(owned, budget, self.admitted)?),
                base_rendered: self.base_rendered.clone(),
                admitted: true,
            }),
            SequenceMode::Layered {
                base,
                delta,
                joined,
            } => {
                if self.admitted && base.allocation_budget().identity() == budget.identity() {
                    return Ok(self.clone());
                }
                let mut next = Self::from_base(buffer(base, budget, self.admitted)?);
                next.admitted = true;
                if let Some(delta) = delta {
                    // Admit its own parts before the join clones any scalar.
                    next.extend(
                        buffer(delta, budget, self.admitted)?
                            .as_slice()
                            .iter()
                            .cloned(),
                    )?;
                }
                let _ = joined;
                Ok(next)
            }
        }
    }

    /// Attach a shared render cache for the `base` portion.
    /// Subsequent `render_prompt` calls will reuse the memoized `RenderedPrompt` for the base
    /// instead of rewalking it.
    /// The delta is always re-rendered because it changes per LLM iteration.
    pub fn with_base_render_cache(mut self, cache: Arc<BaseRenderCache>) -> Self {
        self.base_rendered = Some(cache);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.len(),
            SequenceMode::Layered { base, delta, .. } => {
                base.len() + delta.as_ref().map_or(0, AppendVec::len)
            }
        }
    }

    /// The messages `next` adds on top of this sequence, when `next` extends
    /// this sequence without rewriting any of its prefix.
    ///
    /// The agreement is *witnessed by construction* rather than computed: a
    /// base/delta rope whose `base` is the same view of the same buffer as
    /// this sequence's `base` shares that prefix by identity, so deciding is a pointer
    /// comparison plus a walk of the (turn-sized) delta — never a walk of the
    /// session's history.
    ///
    /// Returns `None` when the witness is absent: either side rebuilt itself
    /// into an owned list (`make_mut`/`replace`, i.e. a plugin deliberately
    /// rewrote history), the bases are different allocations, or `next`'s
    /// delta diverges from this one's. `None` means "cannot decide cheaply",
    /// so callers fall back to reconciling content.
    pub fn preserved_extension_delta<'a>(&self, next: &'a Self) -> Option<&'a [Message]> {
        let (
            SequenceMode::Layered {
                base: self_base,
                delta: self_delta,
                ..
            },
            SequenceMode::Layered {
                base: next_base,
                delta: next_delta,
                ..
            },
        ) = (&self.mode, &next.mode)
        else {
            return None;
        };
        if !AppendVec::ptr_eq(self_base, next_base) {
            return None;
        }
        let self_delta = self_delta.as_ref().map_or(&[][..], AppendVec::as_slice);
        let next_delta = next_delta.as_ref().map_or(&[][..], AppendVec::as_slice);
        let tail = next_delta.get(self_delta.len()..)?;
        self_delta
            .iter()
            .zip(next_delta.iter())
            .all(|(current, candidate)| message_content_equal(current, candidate))
            .then_some(tail)
    }

    /// Whether this sequence is `base` followed by its own messages,
    /// witnessed by construction rather than compared.
    pub fn has_base(&self, base: &AppendVec<Message>) -> bool {
        matches!(&self.mode, SequenceMode::Layered { base: own, .. } if AppendVec::ptr_eq(own, base))
    }

    /// The length of this sequence's base when `messages` shares that prefix
    /// by buffer identity. Later appends may already be visible in `messages`;
    /// the shared prefix remains immutable while this sequence holds it.
    pub fn base_prefix_len_in(&self, messages: &AppendVec<Message>) -> Option<usize> {
        let SequenceMode::Layered { base, .. } = &self.mode else {
            return None;
        };
        if base.len() > messages.len() {
            return None;
        }
        let mut prefix = messages.clone();
        prefix.truncate(base.len());
        AppendVec::ptr_eq(base, &prefix).then_some(base.len())
    }

    pub fn iter(&self) -> MessageSequenceIter<'_> {
        match &self.mode {
            SequenceMode::Owned(owned) => MessageSequenceIter::Owned(owned.iter()),
            SequenceMode::Layered { base, delta, .. } => {
                let delta = delta.as_ref().map_or(&[][..], AppendVec::as_slice);
                MessageSequenceIter::Split(base.iter().chain(delta.iter()))
            }
        }
    }

    /// Edits prepare the shared join before publication, so every read is
    /// allocation-free and cannot conceal an allocation refusal.
    fn materialize(&self) -> &AppendVec<Message> {
        match &self.mode {
            SequenceMode::Owned(owned) => owned,
            SequenceMode::Layered { base, joined, .. } => joined.as_ref().unwrap_or(base),
        }
    }

    pub fn as_slice(&self) -> &[Message] {
        self.materialize().as_slice()
    }

    pub fn shared(&self) -> AppendVec<Message> {
        self.materialize().clone()
    }

    pub fn make_mut(&mut self) -> Result<&mut [Message], crate::allocation::AllocationError> {
        if matches!(self.mode, SequenceMode::Layered { .. }) {
            let mut owned = self.shared();
            owned.make_mut()?;
            self.mode = SequenceMode::Owned(owned);
        }
        self.base_rendered = None;
        self.admitted = false;
        match &mut self.mode {
            SequenceMode::Owned(owned) => owned.make_mut(),
            SequenceMode::Layered { .. } => {
                unreachable!("the private edit just detached its source")
            }
        }
    }

    /// Truncation only narrows immutable views; even cancellation under
    /// pressure needs no allocation and preserves every held reader.
    pub fn truncate(&mut self, len: usize) {
        match &mut self.mode {
            SequenceMode::Owned(owned) => owned.truncate(len),
            SequenceMode::Layered {
                base,
                delta,
                joined,
            } => {
                if len < base.len() {
                    base.truncate(len);
                    *delta = None;
                    *joined = None;
                    self.base_rendered = None;
                } else {
                    if let Some(delta) = delta {
                        delta.truncate(len - base.len());
                    }
                    if let Some(joined) = joined {
                        joined.truncate(len);
                    }
                }
            }
        }
    }

    /// Ledger for preparing replacement input before its owner retains it.
    pub fn allocation_budget(&self) -> &crate::allocation::AllocationBudget {
        self.materialize().allocation_budget()
    }

    fn adopt_message(
        mut message: Message,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Message, crate::allocation::AllocationError> {
        message.parts = MessageParts::adopt_in(&message.parts, budget)?;
        Ok(message)
    }

    pub fn push(&mut self, message: Message) -> Result<(), crate::allocation::AllocationError> {
        self.extend(std::iter::once(message))
    }

    pub fn extend(
        &mut self,
        messages: impl IntoIterator<Item = Message>,
    ) -> Result<(), crate::allocation::AllocationError> {
        let mut messages = messages.into_iter().peekable();
        if messages.peek().is_none() {
            return Ok(());
        }
        match &mut self.mode {
            SequenceMode::Owned(owned) => {
                let mut draft = owned.clone();
                for message in messages {
                    let message = Self::adopt_message(message, draft.allocation_budget())?;
                    draft.push(message)?;
                }
                *owned = draft;
            }
            SequenceMode::Layered {
                base,
                delta,
                joined,
            } => {
                let budget = base.allocation_budget();
                let mut next_delta = match delta {
                    Some(delta) => delta.clone(),
                    None => AppendVec::try_with_capacity(messages.size_hint().0, budget)?,
                };
                let mut next_joined = joined.as_ref().unwrap_or(base).clone();
                for message in messages {
                    let message = Self::adopt_message(message, budget)?;
                    next_joined.push_cloned_adopting(&message, same_message)?;
                    next_delta.push(message)?;
                }
                *delta = Some(next_delta);
                *joined = Some(next_joined);
            }
        }
        Ok(())
    }

    pub fn replace(
        &mut self,
        messages: Vec<Message>,
    ) -> Result<(), crate::allocation::AllocationError> {
        let budget = self.materialize().allocation_budget();
        let mut next = AppendVec::try_with_capacity(messages.len(), budget)?;
        for message in messages {
            next.push(Self::adopt_message(message, budget)?)?;
        }
        self.mode = SequenceMode::Owned(next);
        self.admitted = true;
        self.base_rendered = None;
        Ok(())
    }

    pub fn render_retained(
        &self,
        policy: &HistoryPresentation,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<crate::llm::types::RequestMessages, crate::allocation::AllocationError> {
        let SequenceMode::Layered { base, delta, .. } = &self.mode else {
            let mut tail = Vec::new();
            append_structured_prompt_tail_owned(self.as_slice(), &[], policy, budget, |message| {
                tail.push(message)
            })?;
            return crate::llm::types::RequestMessages::default().append_rendered(&tail, budget);
        };
        let rendered = match &self.base_rendered {
            Some(cache) => cache.rendered_retained(base.as_slice(), policy, budget)?,
            None => {
                let mut tail = Vec::new();
                append_structured_prompt_tail_owned(
                    base.as_slice(),
                    &[],
                    policy,
                    budget,
                    |message| tail.push(message),
                )?;
                crate::llm::types::RequestMessages::default().append_rendered(&tail, budget)?
            }
        };
        let mut tail = Vec::new();
        append_structured_prompt_tail_owned(
            delta.as_ref().map_or(&[][..], AppendVec::as_slice),
            base.as_slice(),
            policy,
            budget,
            |message| tail.push(message),
        )?;
        rendered.append_rendered(&tail, budget)
    }

    #[expect(
        clippy::expect_used,
        reason = "the public flat adapter uses an unowned budget which cannot refuse"
    )]
    pub fn render_prompt(&self, policy: &HistoryPresentation) -> RenderedPrompt {
        RenderedPrompt {
            messages: self
                .render_retained(policy, &crate::allocation::AllocationBudget::unowned())
                .expect("independent flat render")
                .iter()
                .cloned()
                .collect(),
        }
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
