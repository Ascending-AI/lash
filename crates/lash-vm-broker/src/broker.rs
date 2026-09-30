//! The broker: one run of model code on a worker, with every effect it
//! requests authorised, journaled and performed by the parent.
//!
//! # What the broker owns
//!
//! - **Authority.** Every request is resolved against the admitted context
//!   ([`crate::authority`]); a refused request is answered with a failure the
//!   guest may catch and dispatches nothing.
//! - **Ordinals and identities.** The parent's [`ParentLedger`] gives each
//!   admitted request the run's next ordinal and derives its calls'
//!   `ToolCallId`s (ADR 0117). A request the journal already retains under
//!   that identity with other content is refused before dispatch.
//! - **Fencing.** Every worker frame is admitted through a
//!   [`MessageFence`] for the checkout's lease and the run's owner and frame
//!   epochs, in sequence, and every request id must be new. A stale, replayed
//!   or duplicated message is never applied: the worker is discarded and the
//!   run reports a [`ProtocolViolation`](InfrastructureOutcome::ProtocolViolation).
//! - **Checkpoints.** A run that parks, completes or fails as a guest commits
//!   its VM state and the ledger that matches it as one [`Checkpoint`].
//!   Nothing is committed from a partial frame.
//!
//! # Worker loss
//!
//! When a worker is lost (its stream ends, it is unresponsive, it breaks the
//! protocol), the broker:
//!
//! 1. fences its lease: nothing more is read from it or sent to it, and the
//!    pool discards it;
//! 2. settles the operation it already admitted, within this invocation: an
//!    operation in flight is driven to its journaled outcome, bounded by
//!    [`BrokerBounds::settle_deadline`], and one still unsettled at the bound
//!    is reported parked, to be attached to by its identity, never
//!    dispatched again;
//! 3. returns [`BrokerFailure::WorkerLost`], a typed, retryable
//!    infrastructure failure, so the owning substrate invocation is re-driven.
//!
//! The broker never restarts a run inside a live invocation: its ordinals are
//! positions in the invocation's journal, and a local restart would append new
//! entries after the ones the lost run wrote. The VM and its ordinals are
//! rebuilt only by the substrate replaying the journal (or from a committed
//! checkpoint), where every recorded operation is served with zero dispatch.
//!
//! # Precedence
//!
//! A fully received `Complete` wins over a later end of stream or exit: the
//! broker stops reading when it has one and commits it once. A partial frame
//! is refused, and the last committed checkpoint stands. The end of a stream
//! is the supervisor's evidence, never the worker's testimony.
//!
//! # Cancellation
//!
//! The winner of a cancellation is the journaled instruction-checkpoint
//! observation (ADR 0039): the broker answers each checkpoint request from
//! [`ParentEffects::observe_cancellation`]. A live stop sends a cooperative
//! `Cancel` and kills the worker after [`BrokerBounds::cancel_grace`]; either
//! way, a run the journal did not observe cancelled ends
//! [`BrokerFailure::Interrupted`], and the re-driven invocation's next
//! journaled observation decides. An unsolicited stop is never recorded, so it
//! never changes what a replay branches on.
//!
//! # Slot release
//!
//! A request whose operation needs a worker of its own
//! ([`ParentEffects::needs_worker`]) parks the run first: the broker sends
//! `Park`, commits nothing, releases the slot, performs the operation, checks
//! a worker out again and resumes the run from the parked state, answering
//! the request the run issues again with the outcome it held.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash_vm_protocol::{
    CodecRefusal, ContextDescription, EffectKind, EffectOutcome, EffectRequest, EffectRequestId,
    EffectResponse, EncodedPayload, FrameCodec, FrameEpoch, FrameReader, HeaderRefusal,
    InfrastructureOutcome, MessageFence, OpaqueStateRefusal, OpaqueVmState, ParentFrame,
    ParentMessage, ProgramSource, ProtocolBounds, Start, StartState, StateExpectation,
    SupervisorEvidence, VmContractReads, VmLimits, VmStateKind, WorkerMessage,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::authority::{self, AdmittedContext, AuthorityRefusal, RequestFingerprint};
use crate::effects::{ParentEffects, ParentFault, Performed};
use crate::ledger::{
    AdmittedKind, AdmittedOperation, Checkpoint, CheckpointRefusal, CheckpointStore, ParentLedger,
};
use crate::transport::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots};

/// The bounds a broker holds a run to, beyond the protocol's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrokerBounds {
    pub protocol: ProtocolBounds,
    /// How long a lost worker's admitted operation gets to reach its
    /// journaled outcome before it is reported parked.
    pub settle_deadline: Duration,
    /// How long a stopped run gets to end cooperatively before its worker is
    /// killed.
    pub cancel_grace: Duration,
}

impl BrokerBounds {
    /// Provisional presets, to be finalised against the measurement lane:
    /// the protocol's standard bounds, thirty seconds to settle, and one
    /// second of cancellation grace.
    pub const fn standard() -> Self {
        Self {
            protocol: ProtocolBounds::standard(),
            settle_deadline: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(1),
        }
    }
}

/// The frame a session is in, shared by its runs: opening a frame advances
/// it, and a run under an earlier frame is retired where it stands.
#[derive(Clone, Debug)]
pub struct FrameFence {
    current: watch::Sender<FrameEpoch>,
    /// How many runs are live under each frame.
    live: Arc<watch::Sender<BTreeMap<FrameEpoch, usize>>>,
}

impl FrameFence {
    pub fn new(epoch: FrameEpoch) -> Self {
        Self {
            current: watch::Sender::new(epoch),
            live: Arc::new(watch::Sender::new(BTreeMap::new())),
        }
    }

    pub fn current(&self) -> FrameEpoch {
        *self.current.borrow()
    }

    /// Advances to `epoch`: from here on no response of an earlier frame is
    /// applied, and every run under an earlier frame retires.
    pub fn advance(&self, epoch: FrameEpoch) {
        self.current.send_if_modified(|current| {
            let advanced = epoch > *current;
            if advanced {
                *current = epoch;
            }
            advanced
        });
    }

    /// Resolves once no run of a frame before `epoch` is live.
    pub async fn retired_before(&self, epoch: FrameEpoch) {
        let mut live = self.live.subscribe();
        // The sender lives as long as `self`, so the wait only ends when the
        // runs retire.
        let _ = live
            .wait_for(|runs| runs.range(..epoch).all(|(_, count)| *count == 0))
            .await;
    }

    fn subscribe(&self) -> watch::Receiver<FrameEpoch> {
        self.current.subscribe()
    }

    fn enter(&self, epoch: FrameEpoch) -> LiveRun {
        self.live
            .send_modify(|runs| *runs.entry(epoch).or_default() += 1);
        LiveRun {
            live: Arc::clone(&self.live),
            epoch,
        }
    }
}

/// A run counted live under its frame until it ends.
struct LiveRun {
    live: Arc<watch::Sender<BTreeMap<FrameEpoch, usize>>>,
    epoch: FrameEpoch,
}

impl Drop for LiveRun {
    fn drop(&mut self) {
        self.live.send_modify(|runs| {
            if let Some(count) = runs.get_mut(&self.epoch) {
                *count = count.saturating_sub(1);
            }
        });
    }
}

/// What a run starts from.
#[derive(Clone, Debug)]
pub struct RunStart {
    pub program: ProgramSource,
    pub contexts: Vec<ContextDescription>,
    pub limits: VmLimits,
    /// The committed checkpoint the run resumes from, or `None` to start
    /// fresh with an empty ledger.
    pub from: Option<Checkpoint>,
}

/// An operation settled after its worker was lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledOperation {
    pub ordinal: u64,
    pub call_ids: Vec<lash_sansio::ToolCallId>,
}

/// An operation still unsettled at the settle bound: it stays addressable by
/// its identity, and the re-driven invocation attaches to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedOperation {
    pub ordinal: u64,
    pub call_ids: Vec<lash_sansio::ToolCallId>,
    pub fingerprint: RequestFingerprint,
}

/// What became of the operations a lost run had admitted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settlement {
    pub settled: Vec<SettledOperation>,
    pub parked: Vec<ParkedOperation>,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokeredEnd {
    /// The program finished; its state and ledger are committed.
    Complete {
        value: EncodedPayload,
        checkpoint: Checkpoint,
    },
    /// The guest failed; the state its error semantics keep is committed.
    GuestError {
        error: EncodedPayload,
        checkpoint: Option<Checkpoint>,
    },
    /// The run parked at a boundary; its state and ledger are committed.
    Suspended { checkpoint: Checkpoint },
    /// The journal observed the run cancelled, and it stopped.
    Cancelled,
}

/// Why a run did not end. Every variant is typed; [`Self::is_retryable`]
/// says whether re-driving the owning invocation can succeed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BrokerFailure {
    #[error("the worker was lost: {outcome}")]
    WorkerLost {
        outcome: InfrastructureOutcome,
        settlement: Settlement,
    },
    #[error("the run was stopped before the journal observed its cancellation")]
    Interrupted { settlement: Settlement },
    #[error("no worker could be checked out: {refusal}")]
    Unavailable { refusal: CheckoutRefusal },
    #[error("the run's frame was retired while it ran")]
    FrameRetired,
    #[error("a request drifted from the one its journal retains: {refusal}")]
    RetainedRequestDrift { refusal: AuthorityRefusal },
    #[error("{fault}")]
    Parent { fault: ParentFault },
    #[error("{refusal}")]
    Checkpoint { refusal: CheckpointRefusal },
    #[error("the run's committed state is refused: {refusal}")]
    StateRefused { refusal: OpaqueStateRefusal },
}

impl BrokerFailure {
    /// Whether re-driving the owning invocation can succeed. A drifted
    /// request or a refused committed state fails the same way on every
    /// attempt, and so does a limit the run itself exhausted.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::WorkerLost { outcome, .. } => outcome.is_retryable(),
            Self::Unavailable {
                refusal: CheckoutRefusal::Infrastructure(outcome),
            } => outcome.is_retryable(),
            Self::Interrupted { .. }
            | Self::Unavailable { .. }
            | Self::Parent { .. }
            | Self::Checkpoint { .. } => true,
            Self::FrameRetired | Self::RetainedRequestDrift { .. } | Self::StateRefused { .. } => {
                false
            }
        }
    }

    /// The operations the lost run had admitted, and what became of them.
    pub fn settlement(&self) -> Option<&Settlement> {
        match self {
            Self::WorkerLost { settlement, .. } | Self::Interrupted { settlement } => {
                Some(settlement)
            }
            _ => None,
        }
    }
}

/// One owner's broker.
pub struct Broker<'a> {
    pub context: &'a AdmittedContext,
    pub effects: &'a dyn ParentEffects,
    pub checkpoints: &'a dyn CheckpointStore,
    pub slots: &'a dyn WorkerSlots,
    pub codec: FrameCodec,
    pub contract: VmContractReads,
    pub bounds: BrokerBounds,
    pub frames: FrameFence,
}

/// An operation the run parked on, and the outcome the parent holds for the
/// request the resumed run issues again.
struct HeldOperation {
    fingerprint: RequestFingerprint,
    outcome: EffectOutcome,
}

/// How one checkout's session ended.
enum SessionEnd {
    Ended(BrokeredEnd),
    /// The run parked on `operation` so its slot could be released.
    ParkedForEffect {
        state: OpaqueVmState,
        operation: AdmittedOperation,
    },
    Lost(BrokerFailure),
}

impl Broker<'_> {
    /// Runs the program to its end on the owner's workers, or to the typed
    /// failure the owning invocation acts on. `stop` is the host's live stop.
    pub async fn run(
        &self,
        start: RunStart,
        stop: &CancellationToken,
    ) -> Result<BrokeredEnd, BrokerFailure> {
        let frame_epoch = self.frames.current();
        let mut frames = self.frames.subscribe();
        let _live = self.frames.enter(frame_epoch);
        let (mut ledger, mut state) = match start.from {
            Some(checkpoint) => {
                if checkpoint.frame_epoch != frame_epoch {
                    return Err(BrokerFailure::FrameRetired);
                }
                let kind = checkpoint.vm.kind();
                checkpoint
                    .vm
                    .check(&self.expectation(kind))
                    .map_err(|refusal| BrokerFailure::StateRefused { refusal })?;
                let state = match kind {
                    VmStateKind::Continuation => StartState::Continuation(checkpoint.vm),
                    VmStateKind::Snapshot => StartState::Snapshot(checkpoint.vm),
                };
                (ParentLedger::restore(checkpoint.ledger), state)
            }
            None => (ParentLedger::default(), StartState::Fresh),
        };
        let mut held = None::<HeldOperation>;
        loop {
            let step_start = Start {
                owner: self.context.owner.clone(),
                program: start.program.clone(),
                contexts: start.contexts.clone(),
                state,
                limits: start.limits,
            };
            let checkout = self
                .slots
                .checkout(&self.context.owner, &step_start)
                .await
                .map_err(|refusal| BrokerFailure::Unavailable { refusal })?;
            let mut session = Session {
                broker: self,
                fence: MessageFence::new(checkout.lease, self.context.owner_epoch, frame_epoch),
                outgoing: MessageFence::new(checkout.lease, self.context.owner_epoch, frame_epoch),
                reader: FrameReader::new(self.codec.clone()),
                checkout,
                frame_epoch,
                last_request: None,
                journaled_cancel: false,
                stopping: None,
            };
            let end = session
                .drive(step_start, &mut ledger, &mut held, stop, &mut frames)
                .await;
            let checkout = session.checkout;
            match end {
                SessionEnd::Ended(end) => {
                    self.slots
                        .release(checkout)
                        .await
                        .map_err(|refusal| BrokerFailure::Unavailable { refusal })?;
                    return Ok(end);
                }
                SessionEnd::Lost(failure) => {
                    self.slots
                        .discard(checkout)
                        .await
                        .map_err(|refusal| BrokerFailure::Unavailable { refusal })?;
                    return Err(failure);
                }
                SessionEnd::ParkedForEffect {
                    state: parked,
                    operation,
                } => {
                    // The slot goes back before the operation that needs a
                    // worker of its own runs.
                    self.slots
                        .release(checkout)
                        .await
                        .map_err(|refusal| BrokerFailure::Unavailable { refusal })?;
                    let performed = self
                        .effects
                        .perform(&operation)
                        .await
                        .map_err(|fault| BrokerFailure::Parent { fault })?;
                    let outcome = self.deliverable(&mut ledger, &operation, performed, frame_epoch);
                    held = Some(HeldOperation {
                        fingerprint: operation.fingerprint,
                        outcome,
                    });
                    state = StartState::Continuation(parked);
                }
            }
        }
    }

    fn expectation(&self, kind: VmStateKind) -> StateExpectation<'_> {
        StateExpectation {
            kind,
            owner: &self.context.owner,
            reads: &self.contract,
            max_bytes: self.bounds.protocol.max_vm_state_bytes,
        }
    }

    /// The outcome the worker is answered with: a granted handle is scoped
    /// to the frame, and a value over the bound stays journaled but is
    /// delivered as a typed failure.
    fn deliverable(
        &self,
        ledger: &mut ParentLedger,
        operation: &AdmittedOperation,
        performed: Performed,
        frame_epoch: FrameEpoch,
    ) -> EffectOutcome {
        if let (Some(handle), AdmittedKind::Invoke(call)) = (performed.granted, &operation.kind) {
            ledger.grant(handle, call, operation.ordinal, frame_epoch);
        }
        let limit = self.bounds.protocol.max_effect_value_bytes;
        match &performed.outcome {
            EffectOutcome::Value(value) | EffectOutcome::Failed(value)
                if value.0.len() as u64 > limit =>
            {
                EffectOutcome::Failed(authority::encode_value(&serde_json::json!({
                    "code": "lash_vm_result_too_large",
                    "message": format!(
                        "the result of {} is {} bytes, over the {limit}-byte bound; it stays journaled",
                        operation.command_id(self.context),
                        value.0.len()
                    ),
                })))
            }
            _ => performed.outcome,
        }
    }
}

/// One checkout's conversation with its worker.
struct Session<'b, 'a> {
    broker: &'b Broker<'a>,
    checkout: WorkerCheckout,
    fence: MessageFence,
    outgoing: MessageFence,
    reader: FrameReader,
    frame_epoch: FrameEpoch,
    last_request: Option<EffectRequestId>,
    /// A checkpoint observation during this run answered cancelled.
    journaled_cancel: bool,
    /// When a live stop's grace runs out, once one fired.
    stopping: Option<tokio::time::Instant>,
}

/// Why reading the worker's next message stopped short.
enum ReadStop {
    Lost(InfrastructureOutcome),
    Parent(ParentFault),
    Interrupted,
    FrameRetired,
}

impl Session<'_, '_> {
    async fn drive(
        &mut self,
        start: Start,
        ledger: &mut ParentLedger,
        held: &mut Option<HeldOperation>,
        stop: &CancellationToken,
        frames: &mut watch::Receiver<FrameEpoch>,
    ) -> SessionEnd {
        match self.next_message(stop, frames).await {
            Ok(WorkerMessage::Ready { build }) if &build == self.broker.codec.build() => {}
            Ok(WorkerMessage::Ready { build }) => {
                return self.violation(format!("the worker runs build `{build}`"));
            }
            Ok(other) => return self.violation(format!("the worker opened with {}", name(&other))),
            Err(stop) => return self.stopped(stop, Settlement::default()),
        }
        if let Err(evidence) = self.send(ParentMessage::Start(Box::new(start))).await {
            return self.lost(InfrastructureOutcome::WorkerCrashed { evidence });
        }
        loop {
            let message = match self.next_message(stop, frames).await {
                Ok(message) => message,
                Err(stop) => return self.stopped(stop, Settlement::default()),
            };
            match message {
                WorkerMessage::EffectRequest(request) => {
                    if let Some(end) = self.request(request, ledger, held, stop, frames).await {
                        return end;
                    }
                }
                WorkerMessage::Suspended { state } => {
                    return match self.commit(state, ledger).await {
                        Ok(checkpoint) => SessionEnd::Ended(BrokeredEnd::Suspended { checkpoint }),
                        Err(failure) => SessionEnd::Lost(failure),
                    };
                }
                WorkerMessage::Complete { state, value } => {
                    // A whole `Complete` wins: nothing after it is read.
                    return match self.commit(state, ledger).await {
                        Ok(checkpoint) => {
                            SessionEnd::Ended(BrokeredEnd::Complete { value, checkpoint })
                        }
                        Err(failure) => SessionEnd::Lost(failure),
                    };
                }
                WorkerMessage::GuestError { state, error } => {
                    if self.journaled_cancel {
                        return SessionEnd::Ended(BrokeredEnd::Cancelled);
                    }
                    let checkpoint = match state {
                        Some(state) => match self.commit(state, ledger).await {
                            Ok(checkpoint) => Some(checkpoint),
                            Err(failure) => return SessionEnd::Lost(failure),
                        },
                        None => None,
                    };
                    return SessionEnd::Ended(BrokeredEnd::GuestError { error, checkpoint });
                }
                WorkerMessage::Cancelled if self.journaled_cancel => {
                    return SessionEnd::Ended(BrokeredEnd::Cancelled);
                }
                // A stop the journal never observed decides nothing.
                WorkerMessage::Cancelled => {
                    return SessionEnd::Lost(BrokerFailure::Interrupted {
                        settlement: Settlement::default(),
                    });
                }
                // Phase reports and worker-reported limits never leave
                // `next_message`.
                other @ (WorkerMessage::Observations { .. }
                | WorkerMessage::Refused { .. }
                | WorkerMessage::Ready { .. }
                | WorkerMessage::ResetDone { .. }
                | WorkerMessage::Prepared { .. }
                | WorkerMessage::Progress { .. }
                | WorkerMessage::PayloadTooLarge { .. }
                | WorkerMessage::LimitExceeded { .. }) => {
                    return self.violation(format!("the worker sent {} mid-run", name(&other)));
                }
            }
        }
    }

    /// Handles one effect request; answers the session's end when the
    /// request ended it.
    async fn request(
        &mut self,
        request: EffectRequest,
        ledger: &mut ParentLedger,
        held: &mut Option<HeldOperation>,
        stop: &CancellationToken,
        frames: &mut watch::Receiver<FrameEpoch>,
    ) -> Option<SessionEnd> {
        if self.last_request.is_some_and(|last| request.id <= last) {
            return Some(self.violation(format!(
                "request {:?} repeats an id the run already used",
                request.id
            )));
        }
        self.last_request = Some(request.id);
        let broker = self.broker;
        if request.payload.0.len() as u64 > broker.bounds.protocol.max_effect_value_bytes {
            return Some(self.lost(InfrastructureOutcome::PayloadTooLarge {
                limit: broker.bounds.protocol.max_effect_value_bytes,
                size: request.payload.0.len() as u64,
            }));
        }
        if request.kind == EffectKind::ProjectionRead {
            if let Err(error) = broker.codec.check_payload(&request.payload.0) {
                return Some(self.lost(error.into()));
            }
            let response = match broker.effects.projection(&request.payload) {
                Ok(response) => response,
                Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
            };
            return self
                .answer(
                    request.id,
                    EffectOutcome::Value(response),
                    Settlement::default(),
                )
                .await;
        }
        if request.kind == EffectKind::CancelCheckpoint {
            let Ok(checkpoint) = rmp_serde::from_slice::<u64>(&request.payload.0) else {
                return Some(self.violation("a cancel checkpoint names no checkpoint".into()));
            };
            let cancelled = match broker.effects.observe_cancellation(checkpoint).await {
                Ok(cancelled) => cancelled,
                Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
            };
            self.journaled_cancel |= cancelled;
            return self
                .answer(
                    request.id,
                    EffectOutcome::Checkpoint { cancelled },
                    Settlement::default(),
                )
                .await;
        }
        if request.kind == EffectKind::ProcessBoundary {
            if broker.effects.boundary() {
                return self
                    .send(ParentMessage::Park)
                    .await
                    .err()
                    .map(|evidence| self.lost(InfrastructureOutcome::WorkerCrashed { evidence }));
            }
            return self
                .answer(request.id, EffectOutcome::Unit, Settlement::default())
                .await;
        }
        if request.kind == EffectKind::ParkDeclined {
            let reason = match rmp_serde::from_slice::<String>(&request.payload.0) {
                Ok(reason) => reason,
                Err(error) => {
                    return Some(self.violation(format!("invalid park decline: {error}")));
                }
            };
            broker.effects.park_declined(&reason);
            return self
                .answer(request.id, EffectOutcome::Unit, Settlement::default())
                .await;
        }
        let resolved = match broker.effects.resolve(
            broker.context,
            ledger.grants(),
            self.frame_epoch,
            &request,
        ) {
            Ok(resolved) => resolved,
            Err(refusal) => {
                tracing::warn!(%refusal, owner = %broker.context.owner, "worker request refused");
                return self
                    .answer(
                        request.id,
                        EffectOutcome::Failed(refusal.as_payload()),
                        Settlement::default(),
                    )
                    .await;
            }
        };
        // The resumed run issues the request it parked on again: it is
        // answered with the outcome the parent held, and takes no ordinal.
        if let Some(parked) = held.take() {
            if RequestFingerprint::of(&resolved) != parked.fingerprint {
                return Some(self.violation(
                    "the resumed run issued another request than the one it parked on".into(),
                ));
            }
            return self
                .answer(request.id, parked.outcome, Settlement::default())
                .await;
        }
        let mut operation = ledger.admit(broker.context, resolved);
        operation.request = Some(request.payload.clone());
        match broker.effects.retain(&operation).await {
            Ok(retained) if retained == operation.fingerprint => {}
            Ok(retained) => {
                return Some(SessionEnd::Lost(BrokerFailure::RetainedRequestDrift {
                    refusal: AuthorityRefusal::RetainedRequestDrift {
                        call_id: operation.command_id(broker.context),
                        retained: retained.to_hex(),
                        requested: operation.fingerprint.to_hex(),
                    },
                }));
            }
            Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
        }
        if request.kind.parkable() && broker.effects.needs_worker(&operation) {
            match self.park(request.id, &operation, stop, frames).await {
                ParkAnswer::Parked(state) => {
                    return Some(SessionEnd::ParkedForEffect { state, operation });
                }
                // The worker could not capture its run and, its decline
                // acknowledged, issued the request again: it is performed in
                // place.
                ParkAnswer::Declined(id) => self.last_request = Some(id),
                ParkAnswer::Ended(end) => return Some(*end),
            }
        }
        self.perform(operation, ledger).await
    }

    /// Performs an admitted operation while watching the worker, and answers
    /// the worker with its outcome.
    async fn perform(
        &mut self,
        operation: AdmittedOperation,
        ledger: &mut ParentLedger,
    ) -> Option<SessionEnd> {
        let broker = self.broker;
        let request_id = self.last_request.unwrap_or(EffectRequestId(0));
        let performing = broker.effects.perform(&operation);
        tokio::pin!(performing);
        let lost = loop {
            tokio::select! {
                performed = &mut performing => {
                    let performed = match performed {
                        Ok(performed) => performed,
                        Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
                    };
                    let outcome = broker.deliverable(ledger, &operation, performed, self.frame_epoch);
                    let settled = Settlement {
                        settled: vec![SettledOperation {
                            ordinal: operation.ordinal,
                            call_ids: operation.call_ids(),
                        }],
                        parked: Vec::new(),
                    };
                    return self.answer(request_id, outcome, settled).await;
                }
                read = self.checkout.transport.recv() => match read {
                    WorkerRead::Bytes(bytes) => {
                        if let Err(refusal) = self.reader.push(&bytes) {
                            break InfrastructureOutcome::from(refusal);
                        }
                        match self.reader.next_worker() {
                            Ok(None) => {}
                            Ok(Some(frame)) => {
                                break InfrastructureOutcome::ProtocolViolation {
                                    reason: format!(
                                        "the worker sent {} while its request was performed",
                                        name(&frame.message)
                                    ),
                                };
                            }
                            Err(refusal) => break InfrastructureOutcome::from(refusal),
                        }
                    }
                    WorkerRead::Failed(outcome) => break outcome,
                    WorkerRead::Ended(evidence) => {
                        break InfrastructureOutcome::WorkerCrashed { evidence };
                    }
                    WorkerRead::Unresponsive { silent_ms } => {
                        break InfrastructureOutcome::WorkerUnresponsive { silent_ms };
                    }
                },
            }
        };
        // The worker is gone: fence it, then settle the admitted operation
        // within this invocation, bounded.
        self.checkout.transport.kill().await;
        let settlement =
            match tokio::time::timeout(broker.bounds.settle_deadline, &mut performing).await {
                Ok(Ok(performed)) => {
                    broker.deliverable(ledger, &operation, performed, self.frame_epoch);
                    Settlement {
                        settled: vec![SettledOperation {
                            ordinal: operation.ordinal,
                            call_ids: operation.call_ids(),
                        }],
                        parked: Vec::new(),
                    }
                }
                Ok(Err(fault)) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
                Err(_) => Settlement {
                    settled: Vec::new(),
                    parked: vec![ParkedOperation {
                        ordinal: operation.ordinal,
                        call_ids: operation.call_ids(),
                        fingerprint: operation.fingerprint,
                    }],
                },
            };
        Some(SessionEnd::Lost(BrokerFailure::WorkerLost {
            outcome: lost,
            settlement,
        }))
    }

    /// Asks the worker to park on its pending request `id`.
    async fn park(
        &mut self,
        id: EffectRequestId,
        operation: &AdmittedOperation,
        stop: &CancellationToken,
        frames: &mut watch::Receiver<FrameEpoch>,
    ) -> ParkAnswer {
        if let Err(evidence) = self.send(ParentMessage::Park).await {
            return ParkAnswer::Ended(Box::new(
                self.lost_settling(InfrastructureOutcome::WorkerCrashed { evidence }, operation),
            ));
        }
        match self.next_message(stop, frames).await {
            Ok(WorkerMessage::Suspended { state }) => {
                match state.check(&self.broker.expectation(VmStateKind::Continuation)) {
                    Ok(()) => ParkAnswer::Parked(state),
                    Err(refusal) => ParkAnswer::Ended(Box::new(self.lost_settling(
                        InfrastructureOutcome::ProtocolViolation {
                            reason: format!("the parked state is refused: {refusal}"),
                        },
                        operation,
                    ))),
                }
            }
            // The run could not be captured where it stands. Once the
            // decline is acknowledged, it issues the request it stands on
            // again.
            Ok(WorkerMessage::EffectRequest(declined))
                if declined.kind == EffectKind::ParkDeclined && declined.id > id =>
            {
                let reason = match rmp_serde::from_slice::<String>(&declined.payload.0) {
                    Ok(reason) => reason,
                    Err(error) => {
                        return ParkAnswer::Ended(Box::new(
                            self.violation(format!("invalid park decline: {error}")),
                        ));
                    }
                };
                self.broker.effects.park_declined(&reason);
                self.last_request = Some(declined.id);
                if let Some(end) = self
                    .answer(declined.id, EffectOutcome::Unit, Settlement::default())
                    .await
                {
                    return ParkAnswer::Ended(Box::new(match end {
                        SessionEnd::Lost(BrokerFailure::WorkerLost { outcome, .. }) => {
                            self.lost_settling(outcome, operation)
                        }
                        end => end,
                    }));
                }
                match self.next_message(stop, frames).await {
                    Ok(WorkerMessage::EffectRequest(again))
                        if again.id > declined.id && self.reissues(&again, operation) =>
                    {
                        ParkAnswer::Declined(again.id)
                    }
                    Ok(other) => ParkAnswer::Ended(Box::new(self.lost_settling(
                        InfrastructureOutcome::ProtocolViolation {
                            reason: format!(
                                "the worker followed a declined park with {}",
                                name(&other)
                            ),
                        },
                        operation,
                    ))),
                    Err(stop) => {
                        ParkAnswer::Ended(Box::new(self.stopped_settling(stop, operation)))
                    }
                }
            }
            Ok(other) => ParkAnswer::Ended(Box::new(self.lost_settling(
                InfrastructureOutcome::ProtocolViolation {
                    reason: format!("the worker answered a park with {}", name(&other)),
                },
                operation,
            ))),
            Err(stop) => ParkAnswer::Ended(Box::new(self.stopped_settling(stop, operation))),
        }
    }

    /// Whether `again` is the request `operation` was admitted from, issued
    /// again by a run whose park was declined.
    fn reissues(&self, again: &EffectRequest, operation: &AdmittedOperation) -> bool {
        authority::resolve(
            self.broker.context,
            &Default::default(),
            self.frame_epoch,
            again.kind,
            &again.payload,
        )
        .ok()
        .is_some_and(|resolved| RequestFingerprint::of(&resolved) == operation.fingerprint)
    }

    /// A read stopped with an admitted operation nothing has dispatched yet:
    /// it is reported parked, addressable by its identity.
    fn stopped_settling(&self, stop: ReadStop, operation: &AdmittedOperation) -> SessionEnd {
        let settlement = Settlement {
            settled: Vec::new(),
            parked: vec![ParkedOperation {
                ordinal: operation.ordinal,
                call_ids: operation.call_ids(),
                fingerprint: operation.fingerprint,
            }],
        };
        self.stopped(stop, settlement)
    }

    /// A worker lost with an admitted operation nothing has dispatched yet:
    /// it is reported parked, addressable by its identity.
    fn lost_settling(
        &self,
        outcome: InfrastructureOutcome,
        operation: &AdmittedOperation,
    ) -> SessionEnd {
        SessionEnd::Lost(BrokerFailure::WorkerLost {
            outcome,
            settlement: Settlement {
                settled: Vec::new(),
                parked: vec![ParkedOperation {
                    ordinal: operation.ordinal,
                    call_ids: operation.call_ids(),
                    fingerprint: operation.fingerprint,
                }],
            },
        })
    }

    /// Answers request `id`. A worker that can no longer be written to was
    /// lost after the outcome was journaled: `settled` names what it had.
    async fn answer(
        &mut self,
        id: EffectRequestId,
        outcome: EffectOutcome,
        settled: Settlement,
    ) -> Option<SessionEnd> {
        self.journaled_cancel |= matches!(outcome, EffectOutcome::Cancelled);
        match self
            .send(ParentMessage::EffectResponse(EffectResponse {
                id,
                outcome,
            }))
            .await
        {
            Ok(()) => None,
            Err(evidence) => Some(SessionEnd::Lost(BrokerFailure::WorkerLost {
                outcome: InfrastructureOutcome::WorkerCrashed { evidence },
                settlement: settled,
            })),
        }
    }

    /// Commits the worker's state with the ledger that matches it.
    async fn commit(
        &mut self,
        state: OpaqueVmState,
        ledger: &ParentLedger,
    ) -> Result<Checkpoint, BrokerFailure> {
        let kind = state.kind();
        if let Err(refusal) = state.check(&self.broker.expectation(kind)) {
            let outcome = match refusal {
                OpaqueStateRefusal::TooLarge { limit, len } => {
                    InfrastructureOutcome::PayloadTooLarge { limit, size: len }
                }
                refusal => InfrastructureOutcome::ProtocolViolation {
                    reason: format!("the worker's state is refused: {refusal}"),
                },
            };
            return Err(BrokerFailure::WorkerLost {
                outcome,
                settlement: Settlement::default(),
            });
        }
        let checkpoint = Checkpoint {
            vm: state,
            ledger: ledger.snapshot(),
            frame_epoch: self.frame_epoch,
        };
        self.broker
            .checkpoints
            .commit(&checkpoint)
            .await
            .map_err(|refusal| BrokerFailure::Checkpoint { refusal })?;
        Ok(checkpoint)
    }

    async fn send(&mut self, message: ParentMessage) -> Result<(), SupervisorEvidence> {
        let frame = ParentFrame {
            header: self.outgoing.next_header(),
            message,
        };
        let bytes = match self.broker.codec.encode_parent(&frame) {
            Ok(bytes) => bytes,
            // A frame the parent itself cannot encode is its own fault; the
            // worker is treated as lost so the run is re-driven.
            Err(refusal) => {
                tracing::error!(%refusal, "a parent frame could not be encoded");
                return Err(SupervisorEvidence::EndOfStream);
            }
        };
        self.checkout.transport.send(bytes).await
    }

    /// The worker's next admitted message.
    async fn next_message(
        &mut self,
        stop: &CancellationToken,
        frames: &mut watch::Receiver<FrameEpoch>,
    ) -> Result<WorkerMessage, ReadStop> {
        loop {
            match self.reader.next_worker() {
                Ok(Some(frame)) => {
                    if let Err(refusal) = self.fence.admit(&frame.header) {
                        return Err(ReadStop::Lost(refused_header(refusal)));
                    }
                    match frame.message {
                        // Phase deadlines and CPU accounting belong to the
                        // transport's supervisor; a phase report is not a
                        // step of the run.
                        WorkerMessage::Progress { .. } => continue,
                        WorkerMessage::Observations { payload } => {
                            self.broker
                                .codec
                                .check_payload(&payload.0)
                                .map_err(|error| ReadStop::Lost(error.into()))?;
                            self.broker
                                .effects
                                .observe(&payload)
                                .map_err(ReadStop::Parent)?;
                            continue;
                        }
                        WorkerMessage::Refused { reason } => {
                            return Err(ReadStop::Lost(InfrastructureOutcome::ProtocolViolation {
                                reason,
                            }));
                        }
                        WorkerMessage::PayloadTooLarge { limit, size } => {
                            return Err(ReadStop::Lost(InfrastructureOutcome::PayloadTooLarge {
                                limit,
                                size,
                            }));
                        }
                        WorkerMessage::LimitExceeded { limit } => {
                            return Err(ReadStop::Lost(
                                InfrastructureOutcome::WorkerLimitExceeded { limit },
                            ));
                        }
                        message => return Ok(message),
                    }
                }
                Ok(None) => {}
                Err(refusal) => return Err(ReadStop::Lost(refusal.into())),
            }
            let grace = self.stopping;
            let read = tokio::select! {
                read = self.checkout.transport.recv() => read,
                () = stop.cancelled(), if grace.is_none() => {
                    self.stopping = Some(tokio::time::Instant::now() + self.broker.bounds.cancel_grace);
                    // Cooperative: the worker stops at its next probe, and
                    // the journal still decides nothing from it.
                    if self.send(ParentMessage::Cancel).await.is_err() {
                        return Err(ReadStop::Interrupted);
                    }
                    continue;
                }
                () = sleep_until(grace) => {
                    self.checkout.transport.kill().await;
                    return Err(ReadStop::Interrupted);
                }
                changed = frames.changed() => {
                    if changed.is_err() || *frames.borrow() != self.frame_epoch {
                        self.checkout.transport.kill().await;
                        return Err(ReadStop::FrameRetired);
                    }
                    continue;
                }
            };
            match read {
                WorkerRead::Failed(outcome) => return Err(ReadStop::Lost(outcome)),
                WorkerRead::Bytes(bytes) => {
                    if let Err(refusal) = self.reader.push(&bytes) {
                        return Err(ReadStop::Lost(refusal.into()));
                    }
                }
                WorkerRead::Ended(evidence) => {
                    // A partial frame is refused; whole frames before it
                    // already stood.
                    let reader = std::mem::replace(
                        &mut self.reader,
                        FrameReader::new(self.broker.codec.clone()),
                    );
                    if let Err(refusal) = reader.finish() {
                        tracing::warn!(%refusal, "a lost worker's partial frame was refused");
                    }
                    if self.stopping.is_some() {
                        return Err(ReadStop::Interrupted);
                    }
                    return Err(ReadStop::Lost(InfrastructureOutcome::WorkerCrashed {
                        evidence,
                    }));
                }
                WorkerRead::Unresponsive { silent_ms } => {
                    self.checkout.transport.kill().await;
                    return Err(ReadStop::Lost(InfrastructureOutcome::WorkerUnresponsive {
                        silent_ms,
                    }));
                }
            }
        }
    }

    fn stopped(&self, stop: ReadStop, settlement: Settlement) -> SessionEnd {
        SessionEnd::Lost(match stop {
            ReadStop::Lost(outcome) => BrokerFailure::WorkerLost {
                outcome,
                settlement,
            },
            ReadStop::Interrupted if self.journaled_cancel => {
                return SessionEnd::Ended(BrokeredEnd::Cancelled);
            }
            ReadStop::Interrupted => BrokerFailure::Interrupted { settlement },
            ReadStop::FrameRetired => BrokerFailure::FrameRetired,
            ReadStop::Parent(fault) => BrokerFailure::Parent { fault },
        })
    }

    fn lost(&self, outcome: InfrastructureOutcome) -> SessionEnd {
        SessionEnd::Lost(BrokerFailure::WorkerLost {
            outcome,
            settlement: Settlement::default(),
        })
    }

    fn violation(&self, reason: String) -> SessionEnd {
        self.lost(InfrastructureOutcome::ProtocolViolation { reason })
    }
}

enum ParkAnswer {
    Parked(OpaqueVmState),
    Declined(EffectRequestId),
    Ended(Box<SessionEnd>),
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn refused_header(refusal: HeaderRefusal) -> InfrastructureOutcome {
    InfrastructureOutcome::ProtocolViolation {
        reason: format!("a worker message was refused: {refusal}"),
    }
}

fn name(message: &WorkerMessage) -> &'static str {
    match message {
        WorkerMessage::Progress { .. } => "Progress",
        WorkerMessage::PayloadTooLarge { .. } => "PayloadTooLarge",
        WorkerMessage::LimitExceeded { .. } => "LimitExceeded",
        WorkerMessage::Ready { .. } => "Ready",
        WorkerMessage::EffectRequest(_) => "EffectRequest",
        WorkerMessage::Observations { .. } => "Observations",
        WorkerMessage::Refused { .. } => "Refused",
        WorkerMessage::Suspended { .. } => "Suspended",
        WorkerMessage::Complete { .. } => "Complete",
        WorkerMessage::GuestError { .. } => "GuestError",
        WorkerMessage::Cancelled => "Cancelled",
        WorkerMessage::ResetDone { .. } => "ResetDone",
        WorkerMessage::Prepared { .. } => "Prepared",
    }
}

impl From<CodecRefusal> for ReadStop {
    fn from(refusal: CodecRefusal) -> Self {
        Self::Lost(refusal.into())
    }
}

#[cfg(test)]
mod tests;
