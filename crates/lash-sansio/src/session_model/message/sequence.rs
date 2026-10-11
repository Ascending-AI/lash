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
        delta: Arc<Vec<Message>>,
    },
    Owned(Arc<Vec<Message>>),
}

/// A sequence clone shares its lists and flattened cache. Only a write
/// detaches the edited list and invalidates that holder's cache; an effect
/// body can retain a snapshot without copying the turn's history (FIG-5962).
#[derive(Clone, Debug)]
pub struct MessageSequence {
    mode: SequenceMode,
    materialized: Arc<OnceLock<AppendVec<Message>>>,
    base_rendered: Option<Arc<BaseRenderCache>>,
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
            mode: SequenceMode::Owned(Arc::new(messages)),
            materialized: Arc::default(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base(base: AppendVec<Message>) -> Self {
        Self {
            mode: SequenceMode::Layered {
                base,
                delta: Arc::default(),
            },
            materialized: Arc::default(),
            base_rendered: None,
        }
    }

    pub(crate) fn from_base_and_delta(base: AppendVec<Message>, delta: Vec<Message>) -> Self {
        Self {
            mode: SequenceMode::Layered {
                base,
                delta: Arc::new(delta),
            },
            materialized: Arc::default(),
            base_rendered: None,
        }
    }

    /// Attach a shared render cache for the `base` portion.
    /// Subsequent `render_prompt` calls will reuse the memoized `RenderedPrompt` for the base
    /// instead of rewalking it.
    /// The delta is always re-rendered because it changes per LLM iteration.
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
        if !AppendVec::ptr_eq(self_base, next_base) {
            return None;
        }
        let tail = next_delta.get(self_delta.len()..)?;
        self_delta
            .iter()
            .zip(next_delta.iter())
            .all(|(current, candidate)| message_content_equal(current, candidate))
            .then_some(tail)
    }

    /// Whether this sequence is `base` followed by its own messages,
    /// witnessed by construction rather than compared.
    pub(crate) fn has_base(&self, base: &AppendVec<Message>) -> bool {
        matches!(&self.mode, SequenceMode::Layered { base: own, .. } if AppendVec::ptr_eq(own, base))
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
    /// list once; `Layered` reuses `base` when the delta is empty and
    /// otherwise appends the delta to `base`'s buffer, adopting messages
    /// another holder of that buffer already appended, so the join costs
    /// the delta, not the frame.
    fn materialize(&self) -> &AppendVec<Message> {
        match &self.mode {
            SequenceMode::Owned(owned) => self
                .materialized
                .get_or_init(|| AppendVec::from(owned.as_ref().clone())),
            SequenceMode::Layered { base, delta } if delta.is_empty() => base,
            SequenceMode::Layered { base, delta } => self.materialized.get_or_init(|| {
                let mut combined = base.clone();
                combined.extend_adopting(delta.iter().cloned(), same_message);
                combined
            }),
        }
    }

    pub(crate) fn as_slice(&self) -> &[Message] {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.as_slice(),
            SequenceMode::Layered { .. } => self.materialize().as_slice(),
        }
    }

    pub(crate) fn shared(&self) -> AppendVec<Message> {
        self.materialize().clone()
    }

    pub fn make_mut(&mut self) -> &mut Vec<Message> {
        if matches!(self.mode, SequenceMode::Layered { .. }) {
            let owned = self.materialize_owned();
            self.mode = SequenceMode::Owned(Arc::new(owned));
        }
        self.materialized = Arc::default();
        match &mut self.mode {
            SequenceMode::Owned(owned) => Arc::make_mut(owned),
            SequenceMode::Layered { .. } => unreachable!("mode was just set to Owned"),
        }
    }

    /// The flattened list for a deliberate history rewrite.
    fn materialize_owned(&self) -> Vec<Message> {
        match &self.mode {
            SequenceMode::Owned(owned) => owned.as_ref().clone(),
            SequenceMode::Layered { .. } => self.materialize().to_vec(),
        }
    }

    pub(crate) fn push(&mut self, message: Message) {
        match &mut self.mode {
            SequenceMode::Owned(owned) => Arc::make_mut(owned).push(message),
            SequenceMode::Layered { delta, .. } => Arc::make_mut(delta).push(message),
        }
        self.materialized = Arc::default();
    }

    pub(crate) fn extend(&mut self, messages: Vec<Message>) {
        if messages.is_empty() {
            return;
        }
        match &mut self.mode {
            SequenceMode::Owned(owned) => Arc::make_mut(owned).extend(messages),
            SequenceMode::Layered { delta, .. } => Arc::make_mut(delta).extend(messages),
        }
        self.materialized = Arc::default();
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.mode = SequenceMode::Owned(Arc::new(messages));
        self.materialized = Arc::default();
    }

    pub(crate) fn render_prompt(&self) -> RenderedPrompt {
        let SequenceMode::Layered { base, delta } = &self.mode else {
            return render_prompt(self.as_slice());
        };
        if base.is_empty() {
            return render_prompt(delta.as_slice());
        }
        let mut rendered = match &self.base_rendered {
            Some(cache) => RenderedPrompt {
                messages: cache.rendered(base.as_slice()).to_vec(),
            },
            None => render_prompt(base.as_slice()),
        };
        if !delta.is_empty() {
            append_structured_prompt_tail(delta, base.as_slice(), |message| {
                rendered.messages.push(message);
            });
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
