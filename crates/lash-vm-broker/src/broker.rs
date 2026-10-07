//! The broker: one run of model code on a worker, with every operation it
//! issues authorised, admitted with a snapshot of the VM that issued it, and
//! performed by the parent.
//!
//! # What the broker owns
//!
//! - **Authority.** Every request is resolved against the admitted context
//!   ([`crate::authority`]); a refused request is answered with a failure the
//!   guest may catch and dispatches nothing.
//! - **Identities.** The parent's [`ParentLedger`] gives each issued request
//!   the execution's next admission and derives its calls' `ToolCallId`s
//!   (ADR 0117). The admission's [`OperationId`](crate::OperationId) is
//!   minted when it commits and is stored in the snapshot.
//! - **Quiet points.** A request the VM blocks on (a resource operation, a
//!   batch, an await, a sleep, a signal wait) parks the worker first: the
//!   parent takes the VM's continuation and commits it, with the ledger that
//!   matches it, the operation's admission and its waits, in one
//!   transaction ([`SnapshotStore::commit_quiet_point`]). Only then does the
//!   operation's body run, and its outcome commits before the VM is answered
//!   ([`SnapshotStore::settle`]). A request answered in place (a print, a
//!   projection read, a cancel checkpoint) is recomputed after a crash and
//!   recorded nowhere.
//! - **Fencing.** Every worker frame is admitted through a
//!   [`MessageFence`] for the checkout's lease and the run's owner and frame
//!   epochs, in sequence, and every request id must be new. A stale, replayed
//!   or duplicated message is never applied: the worker is discarded and the
//!   run reports a [`ProtocolViolation`](InfrastructureOutcome::ProtocolViolation).
//! - **Ends.** A run that completes, fails as a guest or parks at a boundary
//!   commits its state and ledger as a quiet point, with how it ended.
//!   Nothing is committed from a partial frame.
//!
//! # Restore
//!
//! A run starts from its latest committed checkpoint
//! ([`RunStart::from`]). A checkpoint that records an end answers it without
//! starting the VM. A VM that stands on an admitted operation is fed that
//! operation's saved outcome by identity ([`SnapshotStore::recover`]):
//! settled, `Interrupted` for a started `Once`, or the body run again for a
//! started `Repeatable` or an operation admitted as no execution. The VM then
//! issues the operation again from its continuation and is answered with
//! that outcome. Nothing earlier runs again, and nothing is re-dispatched.
//!
//! # Worker loss
//!
//! An operation's body never runs while a worker stands on it: the worker
//! has parked and released its slot first. A worker lost while it computes
//! (its stream ends, it is unresponsive, it breaks the protocol) is fenced
//! and discarded, and the run fails [`BrokerFailure::WorkerLost`], typed and
//! retryable: the execution resumes from its latest snapshot, recomputing
//! only the effect-free stretch since it.
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
//! The winner of a cancellation is the parent's observation at an
//! instruction checkpoint (ADR 0039): the broker answers each checkpoint
//! request from [`ParentEffects::observe_cancellation`]. A live stop sends a
//! cooperative `Cancel` and kills the worker after
//! [`BrokerBounds::cancel_grace`]; either way, a run the parent did not
//! observe cancelled ends [`BrokerFailure::Interrupted`].
//!
//! # Slot release
//!
//! A run parked on an operation holds no slot while the operation's body
//! runs: the broker sends `Park`, takes the continuation, releases the slot,
//! commits the quiet point, performs the operation, checks a worker out
//! again and resumes the run from its continuation, answering the request
//! the run issues again with the outcome it holds. An operation the parent
//! hands over ([`EffectOutcome::HandedOver`], a wait that outlives this
//! activation) ends the run [`BrokeredEnd::Suspended`] on the committed
//! quiet point, which a later activation restores.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash_vm_protocol::{
    CodecRefusal, ContextDescription, Detail, EffectKind, EffectOutcome, EffectRequest,
    EffectRequestId, EffectResponse, EncodedPayload, Exchange, FrameCodec, FrameEpoch, FrameReader,
    InfrastructureOutcome, MessageFence, OpaqueStateRefusal, OpaqueVmState, ParentFrame,
    ParentMessage, PayloadKind, ProgramSource, ProtocolBounds, ProtocolBreach, SequenceFault,
    Start, StartState, StateExpectation, VmContractReads, VmLimits, VmStateKind, WorkerMessage,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::authority::{AdmittedContext, AuthorityRefusal, HandleGrant, RequestFingerprint};
use crate::effects::{Admission, ParentEffects, ParentFault, Performed};
use crate::ledger::{
    AdmittedKind, AdmittedOperation, Checkpoint, ParentLedger, QuietPointRefusal, RecordedEnd,
};
use crate::snapshot::{QuietPoint, Recovered, SnapshotStore};
use crate::transport::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots};

/// The bounds a broker holds a run to, beyond the protocol's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrokerBounds {
    pub protocol: ProtocolBounds,
    /// How long a stopped run gets to end cooperatively before its worker is
    /// killed.
    pub cancel_grace: Duration,
}

impl BrokerBounds {
    /// Provisional presets, to be finalised against the measurement lane:
    /// the protocol's standard bounds and one second of cancellation grace.
    pub const fn standard() -> Self {
        Self {
            protocol: ProtocolBounds::standard(),
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
    /// The execution's latest committed checkpoint, to resume from.
    pub from: Option<Checkpoint>,
    /// What an execution with no committed checkpoint starts from: fresh, or
    /// the session's VM snapshot.
    pub fresh: StartState,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokeredEnd {
    /// The program finished; its state, ledger and value are committed.
    Complete {
        value: EncodedPayload,
        checkpoint: Checkpoint,
    },
    /// The guest failed; the state its error semantics keep is committed.
    GuestError {
        error: EncodedPayload,
        checkpoint: Option<Checkpoint>,
    },
    /// The run stopped at a committed quiet point: a boundary, or an
    /// operation a later activation resumes it on.
    Suspended { checkpoint: Checkpoint },
    /// The parent observed the run cancelled, and it stopped.
    Cancelled,
}

/// Why a run did not end. Every variant is typed; [`Self::is_retryable`]
/// says whether resuming the execution from its latest snapshot can
/// succeed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BrokerFailure {
    #[error("the worker was lost: {outcome}")]
    WorkerLost { outcome: InfrastructureOutcome },
    #[error("the run was stopped before the parent observed its cancellation")]
    Interrupted,
    #[error("no worker could be checked out: {refusal}")]
    Unavailable { refusal: CheckoutRefusal },
    #[error("the run's frame was retired while it ran")]
    FrameRetired,
    #[error("{fault}")]
    Parent { fault: ParentFault },
    #[error("{refusal}")]
    Checkpoint { refusal: QuietPointRefusal },
    #[error("the run's committed state is refused: {refusal}")]
    StateRefused { refusal: OpaqueStateRefusal },
}

impl BrokerFailure {
    /// Whether resuming the execution can succeed. A refused committed state
    /// fails the same way on every attempt, and so does a limit the run
    /// itself exhausted.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::WorkerLost { outcome } => outcome.is_retryable(),
            Self::Unavailable {
                refusal: CheckoutRefusal::Infrastructure(outcome),
            } => outcome.is_retryable(),
            Self::Interrupted
            | Self::Unavailable { .. }
            | Self::Parent { .. }
            | Self::Checkpoint { .. } => true,
            Self::FrameRetired | Self::StateRefused { .. } => false,
        }
    }
}

/// One owner's broker.
pub struct Broker<'a> {
    pub context: &'a AdmittedContext,
    pub effects: &'a dyn ParentEffects,
    pub checkpoints: &'a dyn SnapshotStore,
    pub slots: &'a dyn WorkerSlots,
    pub codec: FrameCodec,
    pub contract: VmContractReads,
    pub bounds: BrokerBounds,
    pub frames: FrameFence,
}

/// The outcome the parent holds for the request a resumed run issues again.
struct HeldOperation {
    fingerprint: RequestFingerprint,
    outcome: EffectOutcome,
}

/// A run parked on `operation`, admitted as `admission`.
struct Parked {
    state: OpaqueVmState,
    operation: AdmittedOperation,
    admission: Admission,
}

/// How one checkout's session ended.
enum SessionEnd {
    Ended(Box<BrokeredEnd>),
    /// The run parked on `operation`, admitted as `admission`, so its quiet
    /// point could commit and its slot be released.
    ParkedForEffect(Box<Parked>),
    Lost(BrokerFailure),
}

impl Broker<'_> {
    /// Runs the program to its end on the owner's workers, or to the typed
    /// failure the execution acts on. `stop` is the host's live stop.
    pub async fn run(
        &self,
        start: RunStart,
        stop: &CancellationToken,
    ) -> Result<BrokeredEnd, BrokerFailure> {
        let frame_epoch = self.frames.current();
        let mut frames = self.frames.subscribe();
        let _live = self.frames.enter(frame_epoch);
        let mut held = None::<HeldOperation>;
        // A run with a committed snapshot commits its end over it; an
        // effect-free run commits nothing, and runs again from its start.
        let mut snapshotted = start.from.is_some();
        let (mut ledger, mut state) = match start.from {
            Some(checkpoint) => {
                if checkpoint.frame_epoch() != frame_epoch {
                    return Err(BrokerFailure::FrameRetired);
                }
                // A run that ended answers its end; nothing starts.
                match checkpoint.end.clone() {
                    Some(RecordedEnd::Complete { value }) => {
                        return Ok(BrokeredEnd::Complete { value, checkpoint });
                    }
                    Some(RecordedEnd::GuestError { error }) => {
                        return Ok(BrokeredEnd::GuestError {
                            error,
                            checkpoint: Some(checkpoint),
                        });
                    }
                    None => {}
                }
                let kind = checkpoint.vm.kind();
                checkpoint
                    .vm
                    .check(&self.expectation(kind))
                    .map_err(|refusal| match refusal {
                        refusal @ OpaqueStateRefusal::TooLarge { .. } => {
                            BrokerFailure::WorkerLost {
                                outcome: InfrastructureOutcome::input_state(refusal),
                            }
                        }
                        refusal @ (OpaqueStateRefusal::WrongKind { .. }
                        | OpaqueStateRefusal::WrongOwner { .. }
                        | OpaqueStateRefusal::ComponentOutsideReadRange { .. }
                        | OpaqueStateRefusal::HashMismatch) => {
                            BrokerFailure::StateRefused { refusal }
                        }
                    })?;
                let mut ledger = ParentLedger::restore(checkpoint.ledger.clone());
                match self.restore(&mut ledger, frame_epoch).await? {
                    Restored::Held(operation) => held = Some(operation),
                    Restored::HandedOver => return Ok(BrokeredEnd::Suspended { checkpoint }),
                    Restored::Quiet => {}
                }
                let state = match kind {
                    VmStateKind::Continuation => StartState::Continuation(checkpoint.vm),
                    VmStateKind::Snapshot => StartState::Snapshot(checkpoint.vm),
                };
                (ledger, state)
            }
            None => (ParentLedger::start(frame_epoch), start.fresh),
        };
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
                observed_cancel: false,
                stopping: None,
                snapshotted,
            };
            let end = session
                .drive(step_start, &mut ledger, &mut held, stop, &mut frames)
                .await;
            let checkout = session.checkout;
            match end {
                SessionEnd::Ended(end) => {
                    let end = *end;
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
                SessionEnd::ParkedForEffect(parked) => {
                    let Parked {
                        state: parked,
                        mut operation,
                        admission,
                    } = *parked;
                    // The slot goes back before the operation's body runs.
                    self.slots
                        .release(checkout)
                        .await
                        .map_err(|refusal| BrokerFailure::Unavailable { refusal })?;
                    let committed = self
                        .checkpoints
                        .commit_quiet_point(QuietPoint {
                            checkpoint: Checkpoint {
                                vm: parked.clone(),
                                ledger: ledger.snapshot(),
                                host: self.host_state()?,
                                end: None,
                            },
                            admit: admission.draft,
                            waits: admission.waits,
                            with: Vec::new(),
                        })
                        .await
                        .map_err(|refusal| BrokerFailure::Checkpoint { refusal })?;
                    snapshotted = true;
                    ledger = ParentLedger::restore(committed.checkpoint.ledger.clone());
                    operation.operation = ledger.pending().and_then(|pending| pending.operation);
                    let performed = self
                        .effects
                        .perform(&operation, &committed.waits)
                        .await
                        .map_err(|fault| BrokerFailure::Parent { fault })?;
                    let Some(outcome) = self
                        .settled(&mut ledger, &operation, performed, frame_epoch)
                        .await?
                    else {
                        // The parent left the operation open beyond this
                        // activation: the committed quiet point resumes it.
                        return Ok(BrokeredEnd::Suspended {
                            checkpoint: committed.checkpoint,
                        });
                    };
                    held = Some(HeldOperation {
                        fingerprint: operation.fingerprint,
                        outcome,
                    });
                    state = StartState::Continuation(parked);
                }
            }
        }
    }

    /// Feeds a restored VM the outcome of the operation it stands on, by its
    /// identity. The operation is resolved again from the request the ledger
    /// kept, and must resolve to what was admitted.
    async fn restore(
        &self,
        ledger: &mut ParentLedger,
        frame_epoch: FrameEpoch,
    ) -> Result<Restored, BrokerFailure> {
        let Some(pending) = ledger.pending().cloned() else {
            return Ok(Restored::Quiet);
        };
        let parent = |fault: String| BrokerFailure::Parent {
            fault: ParentFault(fault),
        };
        let recovered = self
            .checkpoints
            .recover(&pending)
            .await
            .map_err(|refusal| BrokerFailure::Checkpoint { refusal })?;
        let request = EffectRequest {
            id: EffectRequestId(0),
            kind: pending.kind,
            payload: pending.request.clone(),
        };
        let resolved = self
            .effects
            .resolve(self.context, ledger.grants(), frame_epoch, &request)
            .map_err(|refusal| parent(format!("the restored operation is refused: {refusal}")))?;
        let operation = ledger.reissue(self.context, resolved, &pending);
        if operation.fingerprint != pending.fingerprint {
            return Err(parent(
                "the restored operation resolves to other content than was admitted".into(),
            ));
        }
        let outcome = match recovered {
            Recovered::Settled(performed) => {
                Some(self.deliverable(ledger, &operation, performed, frame_epoch)?)
            }
            Recovered::Interrupted => {
                ledger.answered();
                Some(self.effects.interrupted(&operation))
            }
            Recovered::Rerun { waits } => {
                let waits = waits
                    .into_iter()
                    .map(|wait| (wait, None))
                    .collect::<Vec<_>>();
                let performed = self
                    .effects
                    .perform(&operation, &waits)
                    .await
                    .map_err(|fault| BrokerFailure::Parent { fault })?;
                self.settled(ledger, &operation, performed, frame_epoch)
                    .await?
            }
        };
        Ok(match outcome {
            Some(outcome) => Restored::Held(HeldOperation {
                fingerprint: pending.fingerprint,
                outcome,
            }),
            None => Restored::HandedOver,
        })
    }

    /// Records what `operation`'s body performed and answers the outcome the
    /// VM is answered with; `None` when the parent handed the operation
    /// over, leaving it open beyond this activation.
    async fn settled(
        &self,
        ledger: &mut ParentLedger,
        operation: &AdmittedOperation,
        performed: Performed,
        frame_epoch: FrameEpoch,
    ) -> Result<Option<EffectOutcome>, BrokerFailure> {
        if performed.outcome == EffectOutcome::HandedOver {
            if operation.operation.is_some() {
                return Err(BrokerFailure::Parent {
                    fault: ParentFault(
                        "an operation admitted as an execution cannot be handed over".into(),
                    ),
                });
            }
            return Ok(None);
        }
        if let Some(id) = operation.operation {
            self.checkpoints
                .settle(id, &performed)
                .await
                .map_err(|refusal| BrokerFailure::Checkpoint { refusal })?;
        }
        self.deliverable(ledger, operation, performed, frame_epoch)
            .map(Some)
    }

    fn host_state(&self) -> Result<Option<EncodedPayload>, BrokerFailure> {
        self.effects
            .host_state()
            .map_err(|fault| BrokerFailure::Parent { fault })
    }

    fn expectation(&self, kind: VmStateKind) -> StateExpectation<'_> {
        StateExpectation {
            kind,
            owner: &self.context.owner,
            reads: &self.contract,
            max_bytes: self.bounds.protocol.max_vm_state_bytes,
        }
    }

    /// The outcome the worker is answered with, once the VM no longer
    /// stands on `operation`: a granted handle is scoped to the frame, and a
    /// value over the bound stays recorded but ends the run with a typed
    /// run limit.
    fn deliverable(
        &self,
        ledger: &mut ParentLedger,
        operation: &AdmittedOperation,
        performed: Performed,
        frame_epoch: FrameEpoch,
    ) -> Result<EffectOutcome, BrokerFailure> {
        if let (Some(handle), AdmittedKind::Invoke(call)) = (performed.granted, &operation.kind) {
            ledger.grant(handle, call, operation.run, frame_epoch);
        }
        ledger.answered();
        let limit = self.bounds.protocol.max_effect_value_bytes;
        match &performed.outcome {
            EffectOutcome::Value(value) | EffectOutcome::Failed(value)
                if value.0.len() as u64 > limit =>
            {
                Err(BrokerFailure::WorkerLost {
                    outcome: InfrastructureOutcome::WorkerLimitExceeded {
                        limit: lash_vm_protocol::WorkerLimit::EffectValue {
                            size: value.0.len() as u64,
                            bound: limit,
                        },
                    },
                })
            }
            _ => Ok(performed.outcome),
        }
    }
}

/// What restoring a checkpoint's pending operation left.
enum Restored {
    /// The VM stood on nothing.
    Quiet,
    /// The outcome the re-issued request is answered with.
    Held(HeldOperation),
    /// The parent left the operation open again: the run stays suspended.
    HandedOver,
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
    observed_cancel: bool,
    /// When a live stop's grace runs out, once one fired.
    stopping: Option<tokio::time::Instant>,
    /// Whether the run has a committed snapshot its end replaces.
    snapshotted: bool,
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
            Ok(WorkerMessage::Ready {
                protocol_version,
                crate_version,
            }) => {
                if let Err(refusal) = lash_vm_protocol::check_worker_protocol_version(
                    protocol_version,
                    env!("CARGO_PKG_VERSION"),
                    &crate_version,
                ) {
                    return self.violation(ProtocolBreach::Version { refusal });
                }
            }
            Ok(other) => return self.unexpected(Exchange::Handshake, &other),
            Err(stop) => return self.stopped(stop),
        }
        if let Err(outcome) = self.send(ParentMessage::Start(Box::new(start))).await {
            return self.lost(outcome);
        }
        loop {
            let message = match self.next_message(stop, frames).await {
                Ok(message) => message,
                Err(stop) => return self.stopped(stop),
            };
            match message {
                WorkerMessage::EffectRequest(request) => {
                    if let Some(end) = self.request(request, ledger, held, stop, frames).await {
                        return end;
                    }
                }
                WorkerMessage::Suspended { state } => {
                    return match self.commit(state, ledger, None).await {
                        Ok(checkpoint) => {
                            SessionEnd::Ended(Box::new(BrokeredEnd::Suspended { checkpoint }))
                        }
                        Err(failure) => SessionEnd::Lost(failure),
                    };
                }
                WorkerMessage::Complete { state, value } => {
                    // A whole `Complete` wins: nothing after it is read.
                    let end = RecordedEnd::Complete {
                        value: value.clone(),
                    };
                    return match self.commit(state, ledger, Some(end)).await {
                        Ok(checkpoint) => {
                            SessionEnd::Ended(Box::new(BrokeredEnd::Complete { value, checkpoint }))
                        }
                        Err(failure) => SessionEnd::Lost(failure),
                    };
                }
                WorkerMessage::GuestError { state, error } => {
                    if self.observed_cancel {
                        return SessionEnd::Ended(Box::new(BrokeredEnd::Cancelled));
                    }
                    let checkpoint = match state {
                        Some(state) => {
                            let end = RecordedEnd::GuestError {
                                error: error.clone(),
                            };
                            match self.commit(state, ledger, Some(end)).await {
                                Ok(checkpoint) => Some(checkpoint),
                                Err(failure) => return SessionEnd::Lost(failure),
                            }
                        }
                        None => None,
                    };
                    return SessionEnd::Ended(Box::new(BrokeredEnd::GuestError {
                        error,
                        checkpoint,
                    }));
                }
                WorkerMessage::Cancelled if self.observed_cancel => {
                    return SessionEnd::Ended(Box::new(BrokeredEnd::Cancelled));
                }
                // A stop the parent never observed decides nothing.
                WorkerMessage::Cancelled => {
                    return SessionEnd::Lost(BrokerFailure::Interrupted);
                }
                // Phase reports and worker-reported limits never leave
                // `next_message`.
                other @ (WorkerMessage::ExchangeTiming { .. }
                | WorkerMessage::Observations { .. }
                | WorkerMessage::Refused { .. }
                | WorkerMessage::Ready { .. }
                | WorkerMessage::ResetDone { .. }
                | WorkerMessage::Prepared { .. }
                | WorkerMessage::Progress { .. }
                | WorkerMessage::LimitExceeded { .. }) => {
                    return self.unexpected(Exchange::Run, &other);
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
            return Some(self.violation(SequenceFault::RepeatedRequestId));
        }
        self.last_request = Some(request.id);
        let broker = self.broker;
        if request.payload.0.len() as u64 > broker.bounds.protocol.max_effect_value_bytes {
            return Some(self.lost(InfrastructureOutcome::WorkerLimitExceeded {
                limit: lash_vm_protocol::WorkerLimit::EffectValue {
                    size: request.payload.0.len() as u64,
                    bound: broker.bounds.protocol.max_effect_value_bytes,
                },
            }));
        }
        if request.kind == EffectKind::ProjectionRead {
            if let Err(error) = broker.codec.check_payload(&request.payload.0) {
                return Some(self.lost(error.into()));
            }
            let response = match broker.effects.projection(&request.payload).await {
                Ok(response) => response,
                Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
            };
            return self
                .answer(request.id, EffectOutcome::Value(response))
                .await;
        }
        if request.kind == EffectKind::CancelCheckpoint {
            let checkpoint = match rmp_serde::from_slice::<u64>(&request.payload.0) {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    return Some(self.violation(ProtocolBreach::Payload {
                        payload: PayloadKind::CancelCheckpoint,
                        detail: Detail::new(error),
                    }));
                }
            };
            let cancelled = match broker.effects.observe_cancellation(checkpoint).await {
                Ok(cancelled) => cancelled,
                Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
            };
            self.observed_cancel |= cancelled;
            return self
                .answer(request.id, EffectOutcome::Checkpoint { cancelled })
                .await;
        }
        if request.kind == EffectKind::ProcessBoundary {
            if broker.effects.boundary() {
                return self
                    .send(ParentMessage::Park)
                    .await
                    .err()
                    .map(|outcome| self.lost(outcome));
            }
            return self.answer(request.id, EffectOutcome::Unit).await;
        }
        if request.kind == EffectKind::ParkDeclined {
            let reason = match rmp_serde::from_slice::<String>(&request.payload.0) {
                Ok(reason) => reason,
                Err(error) => {
                    return Some(self.violation(ProtocolBreach::Payload {
                        payload: PayloadKind::ParkDecline,
                        detail: Detail::new(error),
                    }));
                }
            };
            broker.effects.park_declined(&reason);
            return self.answer(request.id, EffectOutcome::Unit).await;
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
                    .answer(request.id, EffectOutcome::Failed(refusal.as_payload()))
                    .await;
            }
        };
        // The resumed run issues the request it parked on again: it is
        // answered with the outcome the parent holds, and takes no admission.
        if let Some(parked) = held.take() {
            if RequestFingerprint::of(&resolved) != parked.fingerprint {
                return Some(self.violation(SequenceFault::ResumedRequestChanged));
            }
            return self.answer(request.id, parked.outcome).await;
        }
        let operation = ledger.issue(broker.context, resolved, &request);
        if !request.kind.parkable() {
            // Answered in place: recomputed after a crash, recorded nowhere.
            let performed = match broker.effects.perform(&operation, &[]).await {
                Ok(performed) => performed,
                Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
            };
            return match broker.deliverable(ledger, &operation, performed, self.frame_epoch) {
                Ok(outcome) => self.answer(request.id, outcome).await,
                Err(failure) => Some(SessionEnd::Lost(failure)),
            };
        }
        let admission = match broker.effects.admission(&operation).await {
            Ok(admission) => admission,
            Err(fault) => return Some(SessionEnd::Lost(BrokerFailure::Parent { fault })),
        };
        match self
            .park(request.id, &operation, ledger, stop, frames)
            .await
        {
            ParkAnswer::Parked(state) => Some(SessionEnd::ParkedForEffect(Box::new(Parked {
                state,
                operation,
                admission,
            }))),
            // The worker could not capture its run where it stands, and has
            // issued the request again: with no snapshot to admit it with,
            // it is refused.
            ParkAnswer::Declined { again, reason } => {
                ledger.answered();
                let refusal = AuthorityRefusal::NotCapturable { reason };
                self.answer(again, EffectOutcome::Failed(refusal.as_payload()))
                    .await
            }
            ParkAnswer::Ended(end) => Some(*end),
        }
    }

    /// Asks the worker to park on its pending request `id`.
    async fn park(
        &mut self,
        id: EffectRequestId,
        operation: &AdmittedOperation,
        ledger: &ParentLedger,
        stop: &CancellationToken,
        frames: &mut watch::Receiver<FrameEpoch>,
    ) -> ParkAnswer {
        if let Err(outcome) = self.send(ParentMessage::Park).await {
            return ParkAnswer::Ended(Box::new(self.lost(outcome)));
        }
        match self.next_message(stop, frames).await {
            Ok(WorkerMessage::Suspended { state }) => {
                match state.check(&self.broker.expectation(VmStateKind::Continuation)) {
                    Ok(()) => ParkAnswer::Parked(state),
                    Err(refusal) => ParkAnswer::Ended(Box::new(
                        self.lost(InfrastructureOutcome::output_state(refusal)),
                    )),
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
                        return ParkAnswer::Ended(Box::new(self.violation(
                            ProtocolBreach::Payload {
                                payload: PayloadKind::ParkDecline,
                                detail: Detail::new(error),
                            },
                        )));
                    }
                };
                self.broker.effects.park_declined(&reason);
                self.last_request = Some(declined.id);
                if let Some(end) = self.answer(declined.id, EffectOutcome::Unit).await {
                    return ParkAnswer::Ended(Box::new(end));
                }
                match self.next_message(stop, frames).await {
                    Ok(WorkerMessage::EffectRequest(again))
                        if again.id > declined.id
                            && self.reissues(&again, operation, ledger.grants()) =>
                    {
                        self.last_request = Some(again.id);
                        ParkAnswer::Declined {
                            again: again.id,
                            reason,
                        }
                    }
                    Ok(other) => {
                        ParkAnswer::Ended(Box::new(self.unexpected(Exchange::DeclinedPark, &other)))
                    }
                    Err(stop) => ParkAnswer::Ended(Box::new(self.stopped(stop))),
                }
            }
            Ok(other) => ParkAnswer::Ended(Box::new(self.unexpected(Exchange::Park, &other))),
            Err(stop) => ParkAnswer::Ended(Box::new(self.stopped(stop))),
        }
    }

    /// Whether `again` is the request `operation` was issued from, issued
    /// again by a run whose park was declined: resolved as issue resolved
    /// it, by the parent's resolver over the ledger's grants.
    fn reissues(
        &self,
        again: &EffectRequest,
        operation: &AdmittedOperation,
        grants: &BTreeMap<String, HandleGrant>,
    ) -> bool {
        self.broker
            .effects
            .resolve(self.broker.context, grants, self.frame_epoch, again)
            .ok()
            .is_some_and(|resolved| RequestFingerprint::of(&resolved) == operation.fingerprint)
    }

    /// Answers request `id`.
    async fn answer(&mut self, id: EffectRequestId, outcome: EffectOutcome) -> Option<SessionEnd> {
        self.observed_cancel |= matches!(outcome, EffectOutcome::Cancelled);
        match self
            .send(ParentMessage::EffectResponse(EffectResponse {
                id,
                outcome,
            }))
            .await
        {
            Ok(()) => None,
            Err(outcome) => Some(self.lost(outcome)),
        }
    }

    /// Commits the worker's state with the ledger that matches it, and how
    /// the run ended, as a quiet point that admits nothing. The end of a run
    /// that never committed a snapshot is not committed: its stretch was
    /// effect-free, and runs again from its start.
    async fn commit(
        &mut self,
        state: OpaqueVmState,
        ledger: &ParentLedger,
        end: Option<RecordedEnd>,
    ) -> Result<Checkpoint, BrokerFailure> {
        let kind = state.kind();
        if let Err(refusal) = state.check(&self.broker.expectation(kind)) {
            return Err(BrokerFailure::WorkerLost {
                outcome: InfrastructureOutcome::output_state(refusal),
            });
        }
        let checkpoint = Checkpoint {
            vm: state,
            ledger: ledger.snapshot(),
            host: self.broker.host_state()?,
            end,
        };
        if checkpoint.end.is_some() && !self.snapshotted {
            return Ok(checkpoint);
        }
        let committed = self
            .broker
            .checkpoints
            .commit_quiet_point(QuietPoint::bare(checkpoint))
            .await
            .map_err(|refusal| BrokerFailure::Checkpoint { refusal })?;
        Ok(committed.checkpoint)
    }

    async fn send(&mut self, message: ParentMessage) -> Result<(), InfrastructureOutcome> {
        let frame = ParentFrame {
            header: self.outgoing.next_header_copy(),
            message,
        };
        let bytes = self
            .broker
            .codec
            .encode_parent(&frame)
            .map_err(InfrastructureOutcome::from)?;
        self.outgoing.next_header();
        self.checkout
            .transport
            .send(bytes)
            .await
            .map_err(|evidence| InfrastructureOutcome::WorkerCrashed { evidence })
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
                        return Err(ReadStop::Lost(ProtocolBreach::from(refusal).into()));
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
                        WorkerMessage::Refused { refusal } => {
                            return Err(ReadStop::Lost(refusal.into_outcome()));
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
                    // the parent still decides nothing from it.
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

    fn stopped(&self, stop: ReadStop) -> SessionEnd {
        SessionEnd::Lost(match stop {
            ReadStop::Lost(outcome) => BrokerFailure::WorkerLost { outcome },
            ReadStop::Interrupted if self.observed_cancel => {
                return SessionEnd::Ended(Box::new(BrokeredEnd::Cancelled));
            }
            ReadStop::Interrupted => BrokerFailure::Interrupted,
            ReadStop::FrameRetired => BrokerFailure::FrameRetired,
            ReadStop::Parent(fault) => BrokerFailure::Parent { fault },
        })
    }

    fn lost(&self, outcome: InfrastructureOutcome) -> SessionEnd {
        SessionEnd::Lost(BrokerFailure::WorkerLost { outcome })
    }

    fn violation(&self, breach: impl Into<ProtocolBreach>) -> SessionEnd {
        self.lost(breach.into().into())
    }

    /// The worker answered `exchange` with a message it does not admit.
    fn unexpected(&self, exchange: Exchange, found: &WorkerMessage) -> SessionEnd {
        self.violation(ProtocolBreach::Unexpected {
            exchange,
            found: found.kind(),
        })
    }
}

enum ParkAnswer {
    Parked(OpaqueVmState),
    Declined {
        again: EffectRequestId,
        reason: String,
    },
    Ended(Box<SessionEnd>),
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl From<CodecRefusal> for ReadStop {
    fn from(refusal: CodecRefusal) -> Self {
        Self::Lost(refusal.into())
    }
}

#[cfg(test)]
mod tests;
