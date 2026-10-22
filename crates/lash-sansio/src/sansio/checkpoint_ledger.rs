//! What a turn's last committed checkpoint stored, for the next to build on
//! (FIG-5955).
//!
//! A phase checkpoint names every chain and value its machine holds, but
//! stores only what no committed checkpoint of the turn stored before it.
//! The machine keeps the chains its last committed checkpoint was stored
//! as, with the host's pin of every blob they are ([`ContentLedger`]), and
//! how far each of its sequences is unchanged since ([`Floors`]): the
//! machine's sequences only grow, except where a stop truncates them or a
//! recorded sync hands them over again, and each of those lowers its floor.
//! A checkpoint then encodes the items past the floor and the open tail it
//! re-joins, and names every blob it keeps by its pin.
//!
//! The pending model request is kept with the chains after its call ends,
//! so the next call's request, which extends it, stores only what it adds:
//! its frame (the request without its messages) and the messages past the
//! ones they share.
//!
//! A restore starts the ledger from the content it read; the host pins it
//! once it knows the nodes that content is stored as. A ledger that cannot
//! pin every blob it names is dropped, and the next checkpoint stores all.

use lash_protocol_types::allocation::{AllocationError, AllocationHold};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::checkpoint_content::{
    Chain, CheckpointContentPin, CheckpointContentRef, TurnCheckpointContent,
};
use super::machine_state::outside_window;
use super::{SavedTurn, TurnCheckpointRestoreError, TurnProtocol, TurnWindow};
use lash_protocol_types::session_model::message::message_content_equal;
use lash_protocol_types::{
    llm::types::LlmMessage, llm::types::LlmRequest, llm::types::RetainedLlmRequest,
};
use lash_protocol_types::{session_model::Message, session_model::MessageSequence};

/// Immutable pins returned to a host, retaining their directory obligation.
pub type CheckpointPins = Arc<
    lash_protocol_types::allocation::Allocated<Vec<(CheckpointContentRef, CheckpointContentPin)>>,
>;

/// How many leading items of each of the machine's sequences are unchanged
/// since a checkpoint: `usize::MAX` for one that only grew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Floors {
    pub(super) messages: usize,
    pub(super) prompt: usize,
    pub(super) events: usize,
}

impl Default for Floors {
    fn default() -> Self {
        Self {
            messages: usize::MAX,
            prompt: usize::MAX,
            events: usize::MAX,
        }
    }
}

/// Which of the machine's sequences changed.
#[derive(Clone, Copy, Debug)]
pub(super) enum Sequence {
    Messages,
    Prompt,
    Events,
}

impl Floors {
    fn lower(&mut self, sequence: Sequence, at: usize) {
        let floor = match sequence {
            Sequence::Messages => &mut self.messages,
            Sequence::Prompt => &mut self.prompt,
            Sequence::Events => &mut self.events,
        };
        *floor = (*floor).min(at);
    }
}

/// A pending model request as a checkpoint stored it: the request, how many
/// of its leading messages are the window's render, its frame and the chain
/// of its other messages.
#[derive(Clone, Debug)]
pub(super) struct StoredRequest {
    pub(super) request: Arc<RetainedLlmRequest>,
    pub(super) rendered_from_window: usize,
    pub(super) frame: CheckpointContentRef,
    pub(super) messages: Chain,
}

impl StoredRequest {
    /// Store `request` past the `window`'s render, building on `base`, the
    /// request an earlier checkpoint of the turn stored.
    pub(super) fn record<E: Clone>(
        request: &Arc<RetainedLlmRequest>,
        window: Option<&TurnWindow<E>>,
        base: Option<&StoredRequest>,
        content: &mut TurnCheckpointContent,
        encoded: &mut usize,
    ) -> Result<Self, AllocationError> {
        let shared = window.map_or(0, |window| window.shared_retained_render(&request.messages));
        let own: Vec<_> = request.messages.iter_from(shared).collect();
        let base = base.filter(|base| base.rendered_from_window == shared);
        let stable = base.map_or(0, |base| {
            base.request
                .messages
                .iter_from(shared)
                .zip(own.iter().copied())
                .take_while(|(before, now)| before == now)
                .count()
        });
        let messages = Chain::of(
            own.iter().copied(),
            own.len(),
            base.map(|base| &base.messages),
            stable,
            content,
            encoded,
        )?;
        *encoded += 1;
        Ok(Self {
            request: Arc::clone(request),
            rendered_from_window: shared,
            frame: content.put_value(&frame(request))?,
            messages,
        })
    }

    /// The request a checkpoint stored as `frame` and `messages`, after the
    /// first `rendered_from_window` messages of `window`'s render.
    pub(super) fn restore<E: Clone>(
        frame: &CheckpointContentRef,
        messages: &CheckpointContentRef,
        rendered_from_window: usize,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<E>>,
        policy: &lash_protocol_types::session_model::message::HistoryPresentation,
    ) -> Result<Self, TurnCheckpointRestoreError> {
        let mut request: LlmRequest = content.value(frame)?;
        if !request.messages.is_empty() {
            return Err(TurnCheckpointRestoreError::IncompatibleFormat {
                message: "a model request's frame holds messages".to_owned(),
            });
        }
        let (own, chain) = content.sequence::<LlmMessage>(messages)?;
        if rendered_from_window > 0 {
            let render = window
                .map(|window| window.render(policy))
                .ok_or_else(|| outside_window("a model request starts with the window's render"))?;
            request.messages = render
                .as_slice()
                .get(..rendered_from_window)
                .ok_or_else(|| {
                    outside_window(
                        "a model request starts with more of the window's render than it has",
                    )
                })?
                .to_vec();
        }
        request.messages.extend(own);
        Ok(Self {
            request: Arc::new(RetainedLlmRequest::from_request(request)),
            rendered_from_window,
            frame: frame.clone(),
            messages: chain,
        })
    }

    fn blobs(&self) -> impl Iterator<Item = &CheckpointContentRef> {
        std::iter::once(&self.frame).chain(self.messages.blobs())
    }
}

/// `request` without its messages.
fn frame(request: &LlmRequest) -> LlmRequest {
    LlmRequest {
        instructions: request.instructions.clone(),
        model: request.model.clone(),
        messages: Vec::new(),
        tools: Arc::clone(&request.tools),
        tool_choice: request.tool_choice.clone(),
        attachment_acceptance: Arc::clone(&request.attachment_acceptance),
        generation: request.generation.clone(),
        scope: request.scope.clone(),
        output_spec: request.output_spec.clone(),
        stream_events: None,
        provider_trace: None,
        attempt_control: Default::default(),
    }
}

/// A message sequence a checkpoint names: its first `from_window` messages
/// are the window's, and the rest is content.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointMessages {
    pub(super) from_window: usize,
    pub(super) rest: CheckpointContentRef,
}

/// A stored message sequence: how many of its messages lead from the window,
/// and the chain of the rest.
pub(super) type StoredMessages = (usize, Chain);

impl CheckpointMessages {
    /// Record `sequence`, storing what it does not share with `window`, on
    /// `base`, the sequence's stored state, whose first `floor` messages are
    /// unchanged.
    pub(super) fn record<E: Clone>(
        sequence: &MessageSequence,
        window: Option<&TurnWindow<E>>,
        base: Option<&StoredMessages>,
        floor: usize,
        content: &mut TurnCheckpointContent,
        encoded: &mut usize,
    ) -> Result<(Self, StoredMessages), AllocationError> {
        let from_window = window.map_or(0, |window| window.shared_messages(sequence));
        let (base, stable) = rest_base(base, from_window, floor);
        let rest = Chain::of(
            sequence.iter().skip(from_window),
            sequence.len() - from_window,
            base,
            stable,
            content,
            encoded,
        )?;
        Ok((
            Self {
                from_window,
                rest: rest.head().clone(),
            },
            (from_window, rest),
        ))
    }

    /// The sequence this records, its leading messages read from `window`.
    pub(super) fn restore<E: Clone>(
        &self,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<E>>,
    ) -> Result<(MessageSequence, StoredMessages), TurnCheckpointRestoreError> {
        let (rest, chain) = content.sequence(&self.rest)?;
        Ok((
            leading(window, self.from_window, rest)?,
            (self.from_window, chain),
        ))
    }
}

/// The prompt view a checkpoint names: the window's first `from_window`
/// messages, then its own `rest`, then the turn's messages from
/// `messages_from` on. The view differs from the turn's messages only where
/// the attachment-omission policies narrowed the window's (ADR 0133); what
/// the turn appends goes to both, so the messages it shares with the turn's
/// are stored once, as the turn's.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointPrompt {
    pub(super) from_window: usize,
    pub(super) rest: CheckpointContentRef,
    pub(super) messages_from: usize,
}

impl CheckpointPrompt {
    /// Record `prompt` over `messages`, the turn's, as
    /// [`CheckpointMessages::record`] does.
    pub(super) fn record<E: Clone>(
        prompt: &MessageSequence,
        messages: &MessageSequence,
        window: Option<&TurnWindow<E>>,
        base: Option<&StoredMessages>,
        floor: usize,
        content: &mut TurnCheckpointContent,
        encoded: &mut usize,
    ) -> Result<(Self, StoredMessages), AllocationError> {
        let from_window = window.map_or(0, |window| window.shared_messages(prompt));
        let view = &prompt.as_slice()[from_window..];
        let shared = view
            .iter()
            .rev()
            .zip(messages.as_slice().iter().rev())
            .take_while(|(view, message)| message_content_equal(*view, *message))
            .count();
        let own = &view[..view.len() - shared];
        let (base, stable) = rest_base(base, from_window, floor);
        let rest = Chain::of(own.iter(), own.len(), base, stable, content, encoded)?;
        Ok((
            Self {
                from_window,
                rest: rest.head().clone(),
                messages_from: messages.len() - shared,
            },
            (from_window, rest),
        ))
    }

    /// The prompt view this records over `messages`, the turn's, restored.
    pub(super) fn restore<E: Clone>(
        &self,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<E>>,
        messages: &MessageSequence,
    ) -> Result<(MessageSequence, StoredMessages), TurnCheckpointRestoreError> {
        let (mut rest, chain): (Vec<Message>, _) = content.sequence(&self.rest)?;
        let shared = messages
            .as_slice()
            .get(self.messages_from..)
            .ok_or_else(|| TurnCheckpointRestoreError::IncompatibleFormat {
                message: format!(
                    "a prompt view continues with the turn's messages from {}, past its {}",
                    self.messages_from,
                    messages.len()
                ),
            })?;
        rest.extend(shared.iter().cloned());
        Ok((
            leading(window, self.from_window, rest)?,
            (self.from_window, chain),
        ))
    }
}

/// The stored chain a sequence's rest builds on, and how many of its items
/// are unchanged: none when the rest starts elsewhere in the sequence.
fn rest_base(
    base: Option<&StoredMessages>,
    from_window: usize,
    floor: usize,
) -> (Option<&Chain>, usize) {
    match base {
        Some((stored_from, chain)) if *stored_from == from_window => {
            (Some(chain), floor.saturating_sub(from_window))
        }
        _ => (None, 0),
    }
}

/// The window's first `from_window` messages followed by `rest`.
fn leading<E: Clone>(
    window: Option<&TurnWindow<E>>,
    from_window: usize,
    rest: Vec<Message>,
) -> Result<MessageSequence, TurnCheckpointRestoreError> {
    if from_window == 0 {
        return Ok(MessageSequence::from_owned(rest));
    }
    match window {
        Some(window) if from_window <= window.messages().len() => window
            .messages_then(from_window, rest)
            .map_err(TurnCheckpointRestoreError::Allocation),
        _ => Err(outside_window(
            "a message sequence starts with more of the window than it has",
        )),
    }
}

/// Every chain and value a checkpoint was stored as.
#[derive(Clone, Debug)]
pub(super) struct StoredChains {
    pub(super) messages: StoredMessages,
    pub(super) prompt: StoredMessages,
    pub(super) events: Chain,
    pub(super) completions: Chain,
    /// The pending model request, or the turn's last one after its call
    /// ended: the next request builds on it.
    pub(super) request: Option<StoredRequest>,
}

impl StoredChains {
    fn blobs(&self) -> impl Iterator<Item = &CheckpointContentRef> {
        self.messages
            .1
            .blobs()
            .chain(self.prompt.1.blobs())
            .chain(self.events.blobs())
            .chain(self.completions.blobs())
            .chain(self.request.iter().flat_map(StoredRequest::blobs))
    }
}

/// Chains a checkpoint was stored as, with the host's pin of each blob.
#[derive(Debug)]
struct Stored {
    _allocation: Vec<AllocationHold>,
    chains: StoredChains,
    pins: BTreeMap<CheckpointContentRef, CheckpointContentPin>,
}

/// What the turn's last committed checkpoint stored, and what the
/// checkpoint in flight would store once its commit is acknowledged.
#[derive(Debug, Default)]
pub(super) struct ContentLedger {
    pub(super) budget: lash_protocol_types::allocation::AllocationBudget,
    committed: Option<Stored>,
    staged: Option<Stored>,
    /// The unchanged prefixes since the committed checkpoint was taken.
    floors: Floors,
    /// The unchanged prefixes since the staged checkpoint was taken.
    staged_floors: Floors,
}

impl ContentLedger {
    pub(super) fn adopt_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, AllocationError> {
        fn adopt(
            stored: &Stored,
            budget: &lash_protocol_types::allocation::AllocationBudget,
        ) -> Result<Stored, AllocationError> {
            let capacity = stored.chains.blobs().fold(4096u64, |bytes, digest| {
                bytes.saturating_add(digest.as_str().len() as u64 + 4096)
            });
            let allocation = budget.reserve(capacity)?;
            let mut chains = stored.chains.clone();
            chains.messages.1 = chains.messages.1.adopt_in(budget)?;
            chains.prompt.1 = chains.prompt.1.adopt_in(budget)?;
            chains.events = chains.events.adopt_in(budget)?;
            chains.completions = chains.completions.adopt_in(budget)?;
            if let Some(request) = &mut chains.request {
                request.request = Arc::new(request.request.adopt_in(budget)?);
                request.messages = request.messages.adopt_in(budget)?;
            }
            Ok(Stored {
                chains,
                pins: stored.pins.clone(),
                _allocation: vec![allocation],
            })
        }
        Ok(Self {
            budget: budget.clone(),
            committed: self
                .committed
                .as_ref()
                .map(|stored| adopt(stored, budget))
                .transpose()?,
            staged: self
                .staged
                .as_ref()
                .map(|stored| adopt(stored, budget))
                .transpose()?,
            floors: self.floors,
            staged_floors: self.staged_floors,
        })
    }

    /// A sequence's items from `at` on changed.
    pub(super) fn rewrote(&mut self, sequence: Sequence, at: usize) {
        self.floors.lower(sequence, at);
        self.staged_floors.lower(sequence, at);
    }

    /// The committed chains a checkpoint builds on, with how far each
    /// sequence is unchanged since.
    pub(super) fn base(&self) -> (Option<&StoredChains>, Floors) {
        match &self.committed {
            Some(committed) => (Some(&committed.chains), self.floors),
            None => (None, Floors::default()),
        }
    }

    /// Stage `chains`, a checkpoint just taken, whose `content` holds every
    /// blob they are stored as: the blobs the committed checkpoint stored
    /// leave `content` for its pins, which this answers.
    pub(super) fn stage(
        &mut self,
        chains: StoredChains,
        content: &mut TurnCheckpointContent,
    ) -> Result<CheckpointPins, AllocationError> {
        let allocation = self.budget.reserve(
            (chains.blobs().count() as u64)
                .saturating_mul(4096)
                .saturating_add(4096),
        )?;
        let committed = self.committed.as_ref().map(|committed| &committed.pins);
        let mut held = BTreeMap::new();
        for digest in chains.blobs() {
            if let Some(pin) = committed.and_then(|pins| pins.get(digest)) {
                held.insert(digest.clone(), *pin);
            }
        }
        content.retain(|digest| !held.contains_key(digest))?;
        let answer = held
            .iter()
            .map(|(digest, pin)| (digest.clone(), *pin))
            .collect();
        self.staged = Some(Stored {
            chains,
            pins: held,
            _allocation: vec![allocation.clone()],
        });
        self.staged_floors = Floors::default();
        Ok(Arc::new(lash_protocol_types::allocation::Allocated::new(
            answer,
            allocation,
            self.budget.clone(),
        )))
    }

    /// Stage `chains`, read back from a restored checkpoint, for the host
    /// to pin.
    pub(super) fn restored(&mut self, chains: StoredChains) {
        self.committed = None;
        self.staged = Some(Stored {
            chains,
            pins: BTreeMap::new(),
            _allocation: Vec::new(),
        });
        self.floors = Floors::default();
        self.staged_floors = Floors::default();
    }

    /// The staged checkpoint is stored, the blobs it added under `pins`:
    /// it is the committed one now. One whose blobs are not all pinned is
    /// dropped, and the next checkpoint stores everything.
    pub(super) fn stored(
        &mut self,
        pins: impl IntoIterator<Item = (CheckpointContentRef, CheckpointContentPin)>,
    ) -> Result<(), AllocationError> {
        // Build a private directory. Refusal leaves the staged/committed
        // checkpoint and every acknowledgement unchanged.
        let Some(staged) = self.staged.as_ref() else {
            return Ok(());
        };
        let directory = self.budget.reserve(
            (staged.pins.len() as u64)
                .saturating_mul(4096)
                .saturating_add(4096),
        )?;
        let mut held = staged.pins.clone();
        let mut allocations = vec![directory];
        for (digest, pin) in pins {
            let allocation = self
                .budget
                .reserve(4096u64.saturating_add(digest.as_str().len() as u64))?;
            held.insert(digest, pin);
            allocations.push(allocation);
        }
        let complete = staged
            .chains
            .blobs()
            .all(|digest| held.contains_key(digest));
        held.retain(|digest, _| staged.chains.blobs().any(|blob| blob == digest));
        let Some(mut staged) = self.staged.take() else {
            return Ok(());
        };
        if complete {
            staged.pins = held;
            staged._allocation = allocations;
            self.committed = Some(staged);
            self.floors = self.staged_floors;
        } else {
            self.committed = None;
            self.floors = Floors::default();
        }
        Ok(())
    }
}

/// A phase checkpoint as [`TurnMachine::checkpoint_delta`] takes it.
///
/// [`TurnMachine::checkpoint_delta`]: super::TurnMachine::checkpoint_delta
#[derive(Debug)]
pub struct CheckpointDelta<M: TurnProtocol> {
    /// The checkpoint, its content only the blobs no committed checkpoint
    /// of the turn stored.
    pub saved: SavedTurn<M>,
    /// The blobs it names that a committed checkpoint stored, by the pin
    /// the host gave each.
    pub held: CheckpointPins,
    /// How many items, and values, the checkpoint encoded.
    pub encoded: usize,
}
