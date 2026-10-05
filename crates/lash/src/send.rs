//! One way in: [`LashSession::send`](crate::LashSession::send) accepts an
//! input durably and asks the session's engine to execute it (FIG-3600).
//!
//! The turn no longer runs in the caller's future. The caller holds a
//! [`SendHandle`] and reads what happened from what was recorded: the input's
//! run, then that run's terminal or its park. The engine is used only as a
//! wake barrier and to surface a shift it refused.
//!
//! Polling any one of a handle's [`events`](SendHandle::events),
//! [`outcome`](SendHandle::outcome) or [`output`](SendHandle::output) is
//! enough to follow the engine to completion. The handle remembers its
//! answer, so a later call answers the same retained outcome.

mod batch;
mod cancel;
mod follow;
mod mailbox;
mod resolve;
#[cfg(feature = "restate")]
pub(crate) mod restate;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::future::BoxFuture;
use lash_core::facade_support::{DurableSessionOps, RuntimeHandle};
use lash_core::runtime::{TurnInputAcceptanceReceipt, TurnInputIngress};
use lash_core::{InputId, LiveReplayStore, SessionId, TurnId};
use lash_sansio::sync::MutexExt;
use tokio::sync::mpsc;

use crate::core::ResolvedQueuedWork;
use crate::durable_session::DurableSession;
use crate::error::{EmbedError, Result, SendError};
use crate::support::{
    EffectHost, LashSession, ProtocolTurnOptions, TurnActivity, TurnActivitySink, TurnInput,
    TurnOutcome,
};
use crate::turn::{TurnOutput, TurnReport};
use lash_core::{GenerationOptions, LlmProfileKey, ReasoningSelection, RunSpec};

use lash_core::facade_support::{
    TurnCancelMode, TurnCancelReceipt, TurnCancelUndeliveredInputPolicy,
};
use lash_core::runtime::PendingTurnInputCancelReceipt;
use lash_core::store::{ParkId, ParkReason, StallReason};

pub use batch::{BatchInput, SendBatchBuilder};
use follow::{Subject, Tap};
pub(crate) use mailbox::{deposit_settled_run, running};

/// The session a send, a handle or a cancel is bound to.
#[derive(Clone)]
pub(crate) enum SendTarget {
    /// An open session: answers also bring its resident runtime to the
    /// committed head.
    Live(LashSession),
    /// A Durable Session: no runtime to refresh.
    Durable(DurableSession),
}

/// What a send reads and writes through: the session's store, its queue
/// operations, the engine a handle waits on, the effect host terminal reads
/// and cancels go through, and the live replay events come from.
#[derive(Clone)]
pub(crate) struct SendParts {
    pub(crate) session_id: SessionId,
    pub(crate) store: lash_core::store::SessionStore,
    pub(crate) ops: DurableSessionOps,
    pub(crate) work: Arc<ResolvedQueuedWork>,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
    /// The models a spec's model key is judged against before the input is
    /// accepted.
    pub(crate) models: Arc<dyn lash_core::LlmProfiles>,
}

/// A target's parts, with the open session's runtime when there is one.
pub(crate) struct SendContext {
    pub(crate) parts: SendParts,
    live: Option<RuntimeHandle>,
}

impl SendContext {
    /// Bring the open session's resident runtime to the committed head, so
    /// its reads reflect what the engine ran. A Durable Session has none.
    async fn refresh(&self) -> Result<()> {
        let Some(runtime) = &self.live else {
            return Ok(());
        };
        let writer = runtime.writer();
        let mut resident = writer.lock().await;
        resident.adopt_committed_head().await?;
        // A cancelled operation can invalidate resident state without moving
        // the head. Restore its services before publishing the host's reads.
        resident.reload_invalidated_resident_session_state().await?;
        runtime.adopt_observation_from(&resident);
        Ok(())
    }

    /// [`refresh`](Self::refresh), unless the shift that deposited
    /// `settled` ran on the open session's own runtime: that runtime holds
    /// the run's commit and published it with the deposit, and its shift
    /// may still hold it while the run's scope closes (FIG-3979).
    async fn refresh_unless_ran_on(&self, settled: Option<&mailbox::SettledRun>) -> Result<()> {
        if let (Some(runtime), Some(settled)) = (&self.live, settled)
            && settled.ran_on(runtime)
        {
            return Ok(());
        }
        self.refresh().await
    }

    /// [`refresh`](Self::refresh) for an answer read while `run`'s run may
    /// still be under way in this process: a run parked for a stopped child
    /// it waits on is answered while its run holds the open session's
    /// runtime (FIG-4618). That run keeps the runtime at the head it commits
    /// and publishes it when it returns, so the answer never waits for it.
    /// Any other holder of the runtime is brief, and is waited for.
    async fn refresh_unless_held_by_run_of(&self, run: &TurnId) -> Result<()> {
        let Some(runtime) = &self.live else {
            return Ok(());
        };
        let writer = runtime.writer();
        let mut resident = match writer.try_lock() {
            Ok(resident) => resident,
            Err(_)
                if mailbox::may_deposit(
                    self.parts.work.store_binding(),
                    &self.parts.session_id,
                    run,
                ) =>
            {
                return Ok(());
            }
            Err(_) => writer.lock().await,
        };
        if resident.adopt_committed_head().await? {
            runtime.adopt_observation_from(&resident);
        }
        Ok(())
    }

    /// The session's state as of the committed head.
    async fn session_snapshot(&self) -> Result<lash_core::SessionSnapshot> {
        if let Some(runtime) = &self.live {
            let writer = runtime.writer();
            let resident = writer.lock().await;
            return Ok(resident.export_state());
        }
        lash_core::store::load_session_window_state(
            &self.parts.store,
            lash_core::store::WindowSelector::Current,
        )
        .await
        .map_err(EmbedError::Store)?
        .map(|loaded| loaded.state.to_snapshot())
        .ok_or_else(|| EmbedError::UnknownSession {
            session_id: self.parts.session_id.clone(),
        })
    }
}

impl SendTarget {
    async fn context(&self) -> Result<SendContext> {
        match self {
            Self::Live(session) => Ok(SendContext {
                parts: session.durable().send_parts().await?,
                live: Some(session.runtime.clone()),
            }),
            Self::Durable(durable) => Ok(SendContext {
                parts: durable.send_parts().await?,
                live: None,
            }),
        }
    }

    fn session_id(&self) -> SessionId {
        match self {
            Self::Live(session) => session.session_id(),
            Self::Durable(durable) => durable.session_id().clone(),
        }
    }

    /// The caller's trace context now, as the core's telemetry adapter sees
    /// it. Synchronous: a send snapshots before its first await.
    pub(crate) fn capture_trace_context(&self) -> Option<lash_core::TraceCarrier> {
        match self {
            Self::Live(session) => session.binding.trace_scopes().capture_current(),
            Self::Durable(durable) => durable.trace_scopes().capture_current(),
        }
    }

    pub(crate) fn begin_host_send(
        &self,
        parent: Option<&lash_core::TraceCarrier>,
    ) -> Option<Box<dyn lash_core::TraceHostOperation>> {
        match self {
            Self::Live(session) => session.binding.trace_scopes().begin_host_send(parent),
            Self::Durable(durable) => durable.trace_scopes().begin_host_send(parent),
        }
    }

    /// The live replay position now: a cursor taken before an acceptance
    /// sees everything the acceptance's shift publishes.
    fn current_cursor(&self) -> lash_core::SessionCursor {
        match self {
            Self::Live(session) => {
                let observation = session.runtime.observe();
                session.runtime.live_replay_store.current_cursor(
                    &SessionId::from(observation.session_id()),
                    observation.session_revision(),
                )
            }
            Self::Durable(durable) => durable
                .live_replay_store()
                .current_cursor(durable.session_id(), lash_core::SessionRevision::new(0)),
        }
    }
}

/// Refuse a spec whose model selection cannot run before the input is
/// accepted: a model key this host's models do not register, or reasoning
/// the capability of the model the run would run refuses, judged as the
/// run will judge it against the session's recorded config (FIG-4531).
/// Nothing is recorded here: the run mints the key's binding once, when it
/// records its shape. A spec that names neither keeps the session's recorded
/// selection and is not judged.
async fn refuse_unservable_selection(context: &SendContext, spec: &RunSpec) -> Result<()> {
    let overrides = &spec.overrides;
    if overrides.model.is_none() && overrides.reasoning.is_none() {
        return Ok(());
    }
    let minted = overrides
        .model
        .as_ref()
        .map(|key| context.parts.models.snapshot(key))
        .transpose()
        .map_err(|error| {
            EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::LlmProfileUnknown,
                format!("send refused: {error}"),
            ))
        })?;
    let selected = match (minted, overrides.reasoning.clone()) {
        (Some(model), Some(reasoning)) => {
            Some(lash_core::LlmProfileConfig::new(model).with_reasoning(reasoning))
        }
        (minted, reasoning) => {
            let recorded = context.session_snapshot().await?.policy.model;
            match (minted, recorded) {
                (Some(model), recorded) => Some(lash_core::LlmProfileConfig {
                    model,
                    reasoning: recorded.map(|model| model.reasoning).unwrap_or_default(),
                }),
                (None, Some(recorded)) => Some(match reasoning {
                    Some(reasoning) => recorded.with_reasoning(reasoning),
                    None => recorded,
                }),
                // A session that records no model refuses the run when its
                // shape resolves.
                (None, None) => None,
            }
        }
    };
    let Some(selected) = selected else {
        return Ok(());
    };
    selected.validate_reasoning().map_err(|error| {
        EmbedError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::ReasoningRefused,
            format!("send refused: {error}"),
        ))
    })
}

// ---------------------------------------------------------------------------
// SendBuilder
// ---------------------------------------------------------------------------

/// Builder for one [`send`](crate::LashSession::send).
///
/// Awaiting it commits the acceptance and asks the engine for a shift; it
/// yields a [`SendHandle`]. [`output`](Self::output) is the one-call form.
///
/// The shape the input runs under is its [`RunSpec`]: the default is the
/// session config as it stands when the input's run starts, after every
/// config command queued ahead of that boundary. [`run`](Self::run) and the
/// one-shot setters shape this input's run only; nothing they set reaches
/// the session config. Inputs whose specs differ never share a turn.
#[must_use = "a SendBuilder does nothing until awaited"]
pub struct SendBuilder {
    pub(crate) target: SendTarget,
    pub(crate) input: TurnInput,
    pub(crate) id: Option<TurnId>,
    pub(crate) ingress: TurnInputIngress,
    pub(crate) run_spec: RunSpec,
    pub(crate) pin: bool,
    pub(crate) trace: SendTraceContext,
}

/// The trace context a send links its input to.
#[derive(Clone, Debug, Default)]
pub(crate) enum SendTraceContext {
    /// Not chosen yet: the acceptance snapshots the caller's context on its
    /// first poll.
    #[default]
    Ambient,
    /// Chosen by the caller, or snapshotted when the caller asked: the
    /// acceptance consults nothing else.
    Captured(Option<lash_core::TraceCarrier>),
}

impl SendTraceContext {
    pub(crate) fn set_context(&mut self, context: lash_core::TraceCarrier) {
        *self = Self::Captured(Some(context));
    }
    pub(crate) fn capture(&mut self, capture: impl FnOnce() -> Option<lash_core::TraceCarrier>) {
        *self = Self::Captured(capture());
    }
    /// The cause the submission carries: a link to the captured context,
    /// snapshotting it from `target` now when none was chosen.
    pub(crate) fn into_context(self, target: &SendTarget) -> Option<lash_core::TraceCarrier> {
        match self {
            Self::Ambient => target.capture_trace_context(),
            Self::Captured(context) => context,
        }
    }

    /// The cause for a submission that captures through
    /// `scopes` directly.
    pub(crate) fn cause_through(
        &self,
        scopes: &dyn lash_core::TraceScopeFactory,
    ) -> lash_core::TraceCause {
        lash_core::TraceCause::linked_to(match self {
            Self::Ambient => scopes.capture_current(),
            Self::Captured(context) => context.clone(),
        })
    }
}

impl SendBuilder {
    pub(crate) fn new(target: SendTarget, input: TurnInput) -> Self {
        Self {
            target,
            input,
            id: None,
            ingress: TurnInputIngress::NextTurn,
            run_spec: RunSpec::default(),
            pin: false,
            trace: SendTraceContext::Ambient,
        }
    }

    /// Link this input to `context`, the trace context of whatever caused
    /// it. An explicit context wins over the caller's ambient one, which is
    /// then never consulted.
    ///
    /// The link is telemetry beside the submission, not part of it: the
    /// first acceptance of an [`id`](Self::id) retains the context it was
    /// given, and a retry under another context is the same submission and
    /// keeps the first one.
    pub fn trace_context(mut self, context: lash_core::TraceCarrier) -> Self {
        self.trace.set_context(context);
        self
    }

    /// Snapshot the caller's current trace context now, through the core's
    /// telemetry adapter, instead of when the send is first polled. Use it
    /// when the builder is awaited somewhere the caller's context no longer
    /// is, such as a spawned task. With no adapter installed there is
    /// nothing to capture and the input is linked to nothing.
    pub fn capture_trace_context(mut self) -> Self {
        self.trace.capture(|| self.target.capture_trace_context());
        self
    }

    /// Pin this input in the transaction that accepts it: the state its
    /// run commits is retained through every collection, from before the
    /// run can start, until it is unpinned or the session is deleted.
    /// [`Target::Input`](lash_core::Target::Input) of
    /// [`SendHandle::input_id`] names it afterwards, for
    /// [`fork_at`](crate::LashCore::fork_at) and
    /// [`unpin`](crate::LashSession::unpin).
    pub fn pin(mut self) -> Self {
        self.pin = true;
        self
    }

    /// The host's id for this input. It is the idempotency key **and** the
    /// run the input starts: it is stored verbatim as the row's source key,
    /// so the input's run is `TurnId(id)`. Under the default drain every
    /// next-turn input is its own run; a drain policy that takes several
    /// inputs into one run executes them under the first one's run.
    ///
    /// A retry validates the original submission digest, including after
    /// settlement. Identical content returns the original acceptance; changed
    /// content is refused.
    ///
    /// This is the recovery for a lost response: a host that does not know
    /// whether a send was accepted sends the same id with the same content
    /// again. That send accepts the input exactly once, however many times it
    /// is repeated, and its handle answers the one run. Reading
    /// [`attach_id`](crate::LashSession::attach_id) instead answers only what
    /// lash holds now: [`SendOutcome::NotAccepted`] when it holds nothing.
    pub fn id(mut self, id: TurnId) -> Self {
        self.id = Some(id);
        self
    }

    /// Where the input applies: [`TurnInputIngress::NextTurn`] (the default)
    /// or an active turn's checkpoint.
    ///
    /// An input steered into a running turn joins that turn's recorded
    /// shape: leave its spec default to inherit it. An explicit spec that
    /// differs from the running turn's is refused before acceptance
    /// ([`RuntimeErrorCode::RunSpecMismatch`](lash_core::RuntimeErrorCode::RunSpecMismatch)).
    pub fn ingress(mut self, ingress: TurnInputIngress) -> Self {
        self.ingress = ingress;
        self
    }

    /// The whole spec this input runs under, replacing anything set before.
    /// The spec is part of the input's submission: a retry under the same
    /// [`id`](Self::id) must carry the same spec.
    pub fn run(mut self, spec: RunSpec) -> Self {
        self.run_spec = spec;
        self
    }

    /// The model this input's run executes on, by the host's key. The run
    /// mints the key's binding once, when it records its shape, and runs the
    /// session's reasoning unless [`reasoning`](Self::reasoning) is set too.
    /// A key the host's models do not register is refused before the input
    /// is accepted.
    pub fn model(mut self, key: impl Into<LlmProfileKey>) -> Self {
        self.run_spec.overrides.model = Some(key.into());
        self
    }

    /// The reasoning this input's run executes its model with.
    pub fn reasoning(mut self, reasoning: ReasoningSelection) -> Self {
        self.run_spec.overrides.reasoning = Some(reasoning);
        self
    }

    /// The generation options this input's run executes with.
    pub fn generation(mut self, generation: GenerationOptions) -> Self {
        self.run_spec.overrides.generation = Some(generation);
        self
    }

    /// The options this input's run states for the session's protocol:
    /// the protocol owner's typed run options (`StandardRunOptions`,
    /// `RlmTurnOptions`), which that owner applies over the session's
    /// recorded namespace. Options the owner's type does not admit refuse
    /// the run's shape. Setting them again replaces them.
    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.run_spec.overrides.protocol_turn_options = Some(options);
        self
    }

    /// Accept, then wait for the settled turn.
    pub async fn output(self) -> Result<TurnOutput> {
        self.await?.output().await
    }

    /// Accept, forward the run's live activity to `sink`, and answer its
    /// [`SendHandle::outcome_into`].
    pub async fn outcome_into(self, sink: &dyn TurnActivitySink) -> Result<SendOutcome> {
        self.await?.outcome_into(sink).await
    }

    /// Accept, forward the run's live activity to `sink`, then return the
    /// settled report ([`SendHandle::output_into`]).
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnReport> {
        self.await?.output_into(sink).await
    }

    async fn accept(self) -> Result<SendHandle> {
        let Self {
            target,
            mut input,
            id,
            ingress,
            run_spec,
            pin,
            trace,
        } = self;
        // The caller's context is snapshotted once, here, on the first poll
        // and before the first await: nothing later changes the edge.
        let parent = trace.into_context(&target);
        let host_send = target.begin_host_send(parent.as_ref());
        let trace_cause = lash_core::TraceCause::linked_to(
            host_send
                .as_ref()
                .map(|operation| operation.carrier())
                .or(parent),
        );
        let context = target.context().await?;
        // The host id names the run; the shift executes the run's turns under
        // it, so the input carries no turn id of its own. An input sent
        // without one gets a fresh id, so its row is keyed and its run named
        // like any other.
        let host_id = id.or_else(|| input.trace_turn_id.take());
        input.trace_turn_id = None;
        let id = Some(host_id.unwrap_or_else(crate::turn::fresh_turn_id));
        let cursor = target.current_cursor();
        refuse_unservable_selection(&context, &run_spec).await?;
        let enqueued = context
            .parts
            .ops
            .enqueue_turn_inputs(
                &context.parts.store,
                vec![(input, id.as_ref().map(ToString::to_string))],
                ingress,
                run_spec,
                pin,
                trace_cause.clone(),
            )
            .await?
            .pop()
            .ok_or_else(|| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    "a batch of one admitted no pending turn input",
                ))
            })?;
        if let Some(operation) = host_send {
            // The unique local carrier is retained only by this acceptance's
            // first SQL writer. A retry returns the original cause instead.
            operation.settle(if enqueued.trace_cause == trace_cause {
                lash_trace::TraceCandidateOutcome::Selected
            } else {
                lash_trace::TraceCandidateOutcome::Reused
            });
        }
        let receipt = TurnInputAcceptanceReceipt::from(&enqueued);
        Ok(SendHandle {
            target,
            receipt,
            id,
            cursor,
            shared: Arc::new(HandleShared::pending()),
        })
    }
}

impl std::future::IntoFuture for SendBuilder {
    type Output = Result<SendHandle>;
    type IntoFuture = BoxFuture<'static, Result<SendHandle>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.accept())
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// The recorded answer to a send. Each variant carries only its own data.
///
/// A run's terminal refusal is a variant, never an `Err`: a handle's
/// `outcome()` answers `Err` when it could not read the answer, which asking
/// again retries, or while the session carries a fault an operator must
/// clear (ADR 0109 §9).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SendOutcome {
    /// An explicitly completed host task, read from its operation receipt.
    OperationSettled {
        run: TurnId,
        outcome: Box<lash_core::runtime::PluginOperationCommandOutcome>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    Settled {
        run: TurnId,
        output: Box<TurnOutput>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    Parked {
        parked: ParkedTurn,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    Stalled {
        stalled: StalledDelivery,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// The run ended with a typed refusal no retry could change, and no turn
    /// of it committed (ADR 0069 §6): the run's recorded terminal answer. A
    /// shift the engine refused before any run took the input answers its
    /// refusal the same way, with no `run`.
    Refused {
        run: Option<TurnId>,
        refusal: Box<lash_core::RuntimeError>,
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// The input was withdrawn before any run took it. Its withdrawal stays
    /// on record, so a send under the same id answers this withdrawal and
    /// runs nothing.
    Withdrawn {
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
    /// Lash holds no record of the input: it was never accepted, or it was
    /// withdrawn and [`vacuum`](lash_core::store::StoreMaintenance::vacuum)
    /// has since reclaimed the withdrawal. A send under the same id is
    /// accepted as new. A host that lost a send's response re-sends the same
    /// id with the same content rather than reading this answer
    /// ([`SendBuilder::id`]).
    NotAccepted {
        gaps: Vec<lash_core::facade_support::LiveReplayGap>,
    },
}

/// How an input's run stands once it stopped moving.
///
/// Parked is not terminal: the run holds its work until an operator
/// redrives, cancels or forks it, and a host re-awaits it through
/// [`LashSession::run`](crate::LashSession::run).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum TurnStatus {
    Answered,
    Failed,
    Cancelled,
    Parked(ParkedTurn),
    /// The input was accepted, but its delivery to the engine stalled (ADR
    /// 0109 §3): no run took it, and none will until an operator re-arms
    /// its obligation. Not terminal: the input stays durable, and a host
    /// re-awaits it once re-armed.
    Stalled(StalledDelivery),
    /// Lash holds no record of the input: see
    /// [`SendOutcome::NotAccepted`].
    NotAccepted,
}

/// An accepted input the engine was never handed: its ingress obligation
/// stalled.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StalledDelivery {
    pub session_id: SessionId,
    pub input_id: InputId,
    pub reason: StallReason,
    /// Delivery attempts made before it stalled.
    pub attempts: u32,
    /// The last delivery failure, as the relay recorded it: its typed code
    /// beside its message.
    pub last_error: Option<lash_core::store::DeliveryError>,
    /// When it stalled, in milliseconds since the Unix epoch.
    pub stalled_at_ms: u64,
}

/// A run that parked (ADR 0104 O3): durable and non-terminal.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParkedTurn {
    pub session_id: SessionId,
    pub run: TurnId,
    pub park_id: ParkId,
    pub reason: ParkReason,
    pub since_ms: u64,
    pub attempts: u32,
}

impl SendOutcome {
    pub fn status(&self) -> TurnStatus {
        match self {
            Self::OperationSettled { outcome, .. } => match outcome.as_ref() {
                lash_core::runtime::PluginOperationCommandOutcome::Completed { .. } => {
                    TurnStatus::Answered
                }
                lash_core::runtime::PluginOperationCommandOutcome::Cancelled => {
                    TurnStatus::Cancelled
                }
                lash_core::runtime::PluginOperationCommandOutcome::Failed { .. }
                | lash_core::runtime::PluginOperationCommandOutcome::Refused { .. } => {
                    TurnStatus::Failed
                }
            },
            Self::Settled { output, .. } => output.status(),
            Self::Parked { parked, .. } => TurnStatus::Parked(parked.clone()),
            Self::Stalled { stalled, .. } => TurnStatus::Stalled(stalled.clone()),
            Self::Refused { .. } => TurnStatus::Failed,
            Self::Withdrawn { .. } => TurnStatus::Cancelled,
            Self::NotAccepted { .. } => TurnStatus::NotAccepted,
        }
    }

    pub fn run(&self) -> Option<&TurnId> {
        match self {
            Self::Settled { run, .. } | Self::OperationSettled { run, .. } => Some(run),
            Self::Parked { parked, .. } => Some(&parked.run),
            Self::Refused { run, .. } => run.as_ref(),
            Self::Stalled { .. } | Self::Withdrawn { .. } | Self::NotAccepted { .. } => None,
        }
    }

    pub fn output(&self) -> Option<&TurnOutput> {
        match self {
            Self::Settled { output, .. } => Some(output),
            Self::OperationSettled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Refused { .. }
            | Self::Withdrawn { .. }
            | Self::NotAccepted { .. } => None,
        }
    }

    /// The typed refusal a [`Refused`](Self::Refused) run ended with.
    pub fn refusal(&self) -> Option<&lash_core::RuntimeError> {
        match self {
            Self::Refused { refusal, .. } => Some(refusal),
            Self::OperationSettled { .. }
            | Self::Settled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Withdrawn { .. }
            | Self::NotAccepted { .. } => None,
        }
    }

    pub fn into_output(self) -> Option<TurnOutput> {
        match self {
            Self::Settled { output, .. } => Some(*output),
            Self::OperationSettled { .. }
            | Self::Parked { .. }
            | Self::Stalled { .. }
            | Self::Refused { .. }
            | Self::Withdrawn { .. }
            | Self::NotAccepted { .. } => None,
        }
    }

    /// Gaps in the follower's live activity. The settled report remains
    /// authoritative even when the follower missed activities.
    pub fn gaps(&self) -> &[lash_core::facade_support::LiveReplayGap] {
        match self {
            Self::OperationSettled { gaps, .. }
            | Self::Settled { gaps, .. }
            | Self::Parked { gaps, .. }
            | Self::Stalled { gaps, .. }
            | Self::Refused { gaps, .. }
            | Self::Withdrawn { gaps }
            | Self::NotAccepted { gaps } => gaps,
        }
    }

    pub(crate) fn gaps_mut(&mut self) -> &mut Vec<lash_core::facade_support::LiveReplayGap> {
        match self {
            Self::OperationSettled { gaps, .. }
            | Self::Settled { gaps, .. }
            | Self::Parked { gaps, .. }
            | Self::Stalled { gaps, .. }
            | Self::Refused { gaps, .. }
            | Self::Withdrawn { gaps }
            | Self::NotAccepted { gaps } => gaps,
        }
    }

    /// Converts the variant to its transport shape. A settled report names
    /// the actual run that answered the input.
    pub fn to_remote(
        &self,
        session_id: &SessionId,
        input_id: &InputId,
    ) -> lash_remote_protocol::RemoteSendOutcome {
        use lash_remote_protocol::{RemoteParkedTurn, RemoteSendOutcome, RemoteStalledDelivery};
        let session_id = session_id.clone();
        let input_id = input_id.to_string();
        let gaps = self.gaps().iter().cloned().map(Into::into).collect();
        match self {
            Self::OperationSettled { run, outcome, .. } => RemoteSendOutcome::OperationSettled {
                session_id,
                input_id,
                run: run.clone(),
                outcome: outcome.as_ref().clone().into(),
                gaps,
            },
            Self::Settled { run, output, .. } => RemoteSendOutcome::Settled {
                report: Box::new(
                    output
                        .result
                        .to_remote(&session_id, run, &output.activities),
                ),
                session_id,
                input_id,
                gaps,
            },
            Self::Parked { parked, .. } => RemoteSendOutcome::Parked {
                session_id,
                input_id,
                parked: RemoteParkedTurn {
                    run: parked.run.clone(),
                    park_id: parked.park_id.feed_sequence(),
                    reason: lash_remote_protocol::RemoteTurnParkReason::from(&parked.reason),
                    since_ms: parked.since_ms,
                    attempts: parked.attempts,
                },
                gaps,
            },
            Self::Stalled { stalled, .. } => RemoteSendOutcome::Stalled {
                session_id,
                input_id,
                stalled: RemoteStalledDelivery {
                    reason: stalled.reason.as_str().to_owned(),
                    attempts: stalled.attempts,
                    code: stalled
                        .last_error
                        .as_ref()
                        .map(|error| error.code.as_str().to_owned()),
                    last_error: stalled
                        .last_error
                        .as_ref()
                        .map(|error| error.message.clone()),
                    stalled_at_ms: stalled.stalled_at_ms,
                },
                gaps,
            },
            Self::Refused { run, refusal, .. } => RemoteSendOutcome::Refused {
                session_id,
                input_id,
                run: run.clone(),
                refusal: Box::new(refusal.as_ref().clone().into()),
                gaps,
            },
            Self::Withdrawn { .. } => RemoteSendOutcome::Withdrawn {
                session_id,
                input_id,
                gaps,
            },
            Self::NotAccepted { .. } => RemoteSendOutcome::NotAccepted {
                session_id,
                input_id,
                gaps,
            },
        }
    }
}

/// The status a committed outcome answers.
pub(crate) fn status_of_outcome(outcome: &TurnOutcome) -> TurnStatus {
    match outcome {
        TurnOutcome::Finished(_)
        | TurnOutcome::AgentFrameSwitch { .. }
        | TurnOutcome::SegmentBoundary { .. } => TurnStatus::Answered,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. }) => {
            TurnStatus::Cancelled
        }
        TurnOutcome::Stopped(_) => TurnStatus::Failed,
    }
}

// ---------------------------------------------------------------------------
// SendHandle and RunHandle
// ---------------------------------------------------------------------------

/// What every call on one handle shares: the answer once it is known.
struct HandleShared {
    settled: Mutex<Option<SendOutcome>>,
}

impl HandleShared {
    fn pending() -> Self {
        Self {
            settled: Mutex::new(None),
        }
    }

    fn answer(&self) -> Option<SendOutcome> {
        self.settled.lock_recover().clone()
    }

    fn remember(&self, outcome: &SendOutcome) {
        let mut settled = self.settled.lock_recover();
        if settled.is_none() {
            *settled = Some(outcome.clone());
        }
    }
}

/// Follow `subject` for a handle: answer from the handle's memory when it
/// already knows, otherwise follow and remember.
async fn settle(
    target: &SendTarget,
    subject: &Subject,
    cursor: &lash_core::SessionCursor,
    shared: &HandleShared,
    mut tap: Tap<'_>,
) -> Result<SendOutcome> {
    if let Some(outcome) = shared.answer() {
        if let Some(output) = outcome.output() {
            for activity in &output.activities {
                tap.activity(activity).await;
            }
        }
        return Ok(outcome);
    }
    let context = target.context().await?;
    let mut from = follow::Position::at(cursor.clone());
    let mut gaps = Vec::new();
    // A follow without a window answers; a pending one would go on from its
    // position.
    let outcome = loop {
        match Box::pin(follow::follow(&context, subject, from, &mut tap, None)).await? {
            follow::Followed::Answered(mut outcome) => {
                gaps.append(outcome.gaps_mut());
                *outcome.gaps_mut() = gaps;
                break *outcome;
            }
            follow::Followed::Pending {
                position,
                gaps: mut met,
            } => {
                from = position;
                gaps.append(&mut met);
            }
        }
    };
    shared.remember(&outcome);
    Ok(outcome)
}

/// An events stream fed by a follower on its own task.
fn spawn_events(
    target: SendTarget,
    subject: Subject,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
) -> TurnEvents {
    let (tx, rx) = mpsc::channel(64);
    let task_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(error) = settle(&target, &subject, &cursor, &shared, Tap::Channel(task_tx)).await
        {
            let _ = tx.send(Err(error)).await;
        }
    });
    TurnEvents {
        inner: Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })),
    }
}

/// An accepted input: follow its live activity, and read how its run
/// answered.
///
/// Dropping a handle stops nothing.
pub struct SendHandle {
    target: SendTarget,
    receipt: TurnInputAcceptanceReceipt,
    id: Option<TurnId>,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
}

impl SendHandle {
    pub fn input_id(&self) -> &InputId {
        &self.receipt.input_id
    }

    /// The receipt of the input's durable acceptance.
    pub fn receipt(&self) -> &TurnInputAcceptanceReceipt {
        &self.receipt
    }

    /// The host id, when the send set [`SendBuilder::id`].
    pub fn id(&self) -> Option<&TurnId> {
        self.id.as_ref()
    }

    /// The live-replay cursor taken before this handle's acceptance: the
    /// position its [`events`](Self::events) and [`outcome`](Self::outcome)
    /// follow from. A reader that journaled the acceptance replays from it
    /// deterministically rather than minting a fresh cursor on replay.
    pub fn cursor(&self) -> &lash_core::SessionCursor {
        &self.cursor
    }

    /// Live activity of the run that applies this input, from the cursor
    /// taken before acceptance. Each call subscribes afresh from that cursor.
    /// It ends once the input's run settles or parks; draining it alone
    /// executes the turn to its answer, which the handle then remembers.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Input(self.receipt.clone()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// The input's run answer: resolves on a terminal **or** a park.
    pub async fn outcome(self) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Input(self.receipt.clone()),
            &self.cursor,
            &self.shared,
            Tap::Quiet,
        )
        .await
    }

    /// [`outcome`](Self::outcome) narrowed to a settled turn. A refused run
    /// answers its typed refusal as [`EmbedError::Runtime`]; a parked run, an
    /// input withdrawn before it ran, or one lash never accepted answers
    /// [`SendError::NotSettled`].
    pub async fn output(self) -> Result<TurnOutput> {
        let input_id = self.receipt.input_id.clone();
        settled_output(input_id, self.outcome().await?)
    }

    /// Forward the run's live activity to `sink` as it arrives, and answer
    /// [`outcome`](Self::outcome) with that activity collected in its
    /// output. The outcome's [`gaps`](SendOutcome::gaps) say where the
    /// forwarded activity is incomplete.
    pub async fn outcome_into(self, sink: &dyn TurnActivitySink) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Input(self.receipt.clone()),
            &self.cursor,
            &self.shared,
            Tap::Sink(sink),
        )
        .await
    }

    /// [`outcome_into`](Self::outcome_into) narrowed to a settled turn's
    /// report, as [`output`](Self::output) narrows [`outcome`](Self::outcome).
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnReport> {
        let input_id = self.receipt.input_id.clone();
        Ok(settled_output(input_id, self.outcome_into(sink).await?)?.result)
    }

    /// Withdraw the input if it is still queued, or cooperatively cancel its
    /// running run.
    pub fn cancel(&self) -> CancelBuilder {
        CancelBuilder::new(
            self.target.clone(),
            CancelTarget::Input(self.receipt.input_id.clone()),
        )
    }

    /// Pin this input: the state the run that applies it commits is
    /// retained through every collection. It can be called before the run
    /// starts, while it runs or after it ended, any number of times; it
    /// never touches the turn. A merged or re-deferred input pins the run
    /// that actually applies it.
    pub async fn pin(&self) -> Result<()> {
        let context = self.target.context().await?;
        context
            .parts
            .store
            .pin(&lash_core::Target::Input(self.receipt.input_id.clone()))
            .await
            .map_err(EmbedError::Store)
    }

    /// The run this input is bound to, once a run has taken it: the run
    /// that executes it, which under a merging drain or a steer is not the
    /// input's own. `None` until then, and for a withdrawn input.
    pub async fn run(&self) -> Result<Option<TurnId>> {
        let context = self.target.context().await?;
        context
            .parts
            .store
            .run_binding(&self.receipt.input_id)
            .await
            .map_err(EmbedError::Store)
    }
}

/// A logical run, re-awaited by id: after a restart, a park verb, or from a
/// handle that only knows the host id.
pub struct RunHandle<Output = serde_json::Value, Error = lash_core::plugin::PluginOperationFailure>
{
    target: SendTarget,
    run: crate::RunId,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
    operation_name: Option<&'static str>,
    decode_error: fn(
        lash_core::plugin::PluginOperationFailure,
    )
        -> std::result::Result<Error, Box<lash_core::plugin::PluginOperationFailure>>,
    output_type: std::marker::PhantomData<fn() -> Output>,
}

impl<Output, Error> Clone for RunHandle<Output, Error> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            run: self.run.clone(),
            cursor: self.cursor.clone(),
            shared: self.shared.clone(),
            operation_name: self.operation_name,
            decode_error: self.decode_error,
            output_type: std::marker::PhantomData,
        }
    }
}

impl<Output, Error> RunHandle<Output, Error> {
    /// The logical Run identity, including for operation Runs.
    pub fn run(&self) -> &crate::RunId {
        &self.run
    }

    /// Request cancellation of this logical owner. Dropping a follower never cancels it.
    pub fn cancel(&self) -> CancelBuilder {
        CancelBuilder::new(
            self.target.clone(),
            CancelTarget::Run(self.run.clone().into()),
        )
    }

    /// Live activity of the run from the moment this handle was made; no
    /// earlier activity is replayed.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Run(self.run.clone().into()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// How the run answers. A run still parked answers Parked again.
    pub async fn outcome(self) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Run(self.run.clone().into()),
            &self.cursor,
            &self.shared,
            Tap::Quiet,
        )
        .await
    }

    pub async fn output(self) -> Result<TurnOutput> {
        let run = self.run.clone();
        let outcome = self.outcome().await?;
        settled_output(InputId::from(run.stored()), outcome)
    }

    async fn raw_result(
        self,
    ) -> Result<lash_core::facade_support::PluginOperationReceipt<serde_json::Value>> {
        let run = self.run.clone();
        let answered = self.outcome().await?;
        let status = answered.status();
        match answered {
            SendOutcome::OperationSettled { outcome, .. } => match *outcome {
                lash_core::runtime::PluginOperationCommandOutcome::Completed {
                    plugin_id,
                    output,
                    events,
                    pending_turn_inputs,
                } => Ok(lash_core::facade_support::PluginOperationReceipt {
                    output,
                    events: events
                        .into_iter()
                        .map(|value| lash_core::facade_support::PluginOwned {
                            plugin_id: plugin_id.clone(),
                            value,
                        })
                        .collect(),
                    pending_turn_inputs,
                }),
                lash_core::runtime::PluginOperationCommandOutcome::Failed { failure } => {
                    Err(EmbedError::Control(
                        lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                    ))
                }
                lash_core::runtime::PluginOperationCommandOutcome::Refused { error } => {
                    Err(EmbedError::Runtime(*error))
                }
                lash_core::runtime::PluginOperationCommandOutcome::Cancelled => {
                    Err(EmbedError::from(SendError::NotSettled {
                        input_id: InputId::from(run.stored()),
                        status,
                    }))
                }
            },
            SendOutcome::Refused { refusal, .. } => Err(EmbedError::Runtime(*refusal)),
            _ => Err(EmbedError::from(SendError::NotSettled {
                input_id: InputId::from(run.stored()),
                status,
            })),
        }
    }
}

impl<Output: serde::de::DeserializeOwned, Error> RunHandle<Output, Error> {
    /// Read a task's receipt using the output and error codecs selected by
    /// `start_task::<Op>`. Unknown error envelopes remain intact. A raw task
    /// handle returns JSON and the original failure envelope.
    pub async fn result(
        self,
    ) -> std::result::Result<
        lash_core::facade_support::PluginOperationReceipt<Output>,
        crate::admin::PluginTaskResultError<Error>,
    > {
        let name = self.operation_name.unwrap_or("raw task");
        let decode_error = self.decode_error;
        let receipt = self.raw_result().await.map_err(|error| {
            use crate::admin::PluginTaskResultError;
            match error {
                EmbedError::Control(
                    lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                ) => match decode_error(*failure.clone()) {
                    Ok(error) => PluginTaskResultError::Failed { error, failure },
                    Err(failure) => PluginTaskResultError::Host(Box::new(EmbedError::Control(
                        lash_core::facade_support::PluginOperationInvokeError::Failed(failure),
                    ))),
                },
                error => PluginTaskResultError::Host(Box::new(error)),
            }
        })?;
        let output = serde_json::from_value(receipt.output).map_err(|error| {
            crate::admin::PluginTaskResultError::Host(Box::new(EmbedError::Plugin(
                lash_core::PluginError::Invoke(format!("invalid {name} output: {error}")),
            )))
        })?;
        Ok(lash_core::facade_support::PluginOperationReceipt {
            output,
            events: receipt.events,
            pending_turn_inputs: receipt.pending_turn_inputs,
        })
    }
}

impl RunHandle {
    pub(crate) fn typed<Op: lash_core::facade_support::PluginTask>(
        self,
    ) -> RunHandle<Op::Output, Op::Error> {
        RunHandle {
            target: self.target,
            run: self.run,
            cursor: self.cursor,
            shared: self.shared,
            operation_name: Some(Op::NAME),
            decode_error: Op::decode_error,
            output_type: std::marker::PhantomData,
        }
    }
}

/// A handle on `input_id`, accepted earlier; its cursor is the observation's
/// current position.
pub(crate) fn attach(target: SendTarget, input_id: InputId) -> SendHandle {
    let session_id = target.session_id();
    let cursor = target.current_cursor();
    SendHandle {
        target,
        receipt: TurnInputAcceptanceReceipt {
            input_id,
            session_id,
            source_key: None,
            ingress: TurnInputIngress::NextTurn,
        },
        id: None,
        cursor,
        shared: Arc::new(HandleShared::pending()),
    }
}

/// A handle on the input a send accepted under host id `id`: a keyed input's
/// id is derived from its session and key, so no read finds it. An id lash
/// holds no record of answers [`SendOutcome::NotAccepted`].
pub(crate) fn attach_id(target: SendTarget, id: TurnId) -> SendHandle {
    let input_id =
        lash_core::PendingTurnInputDraft::keyed_input_id(&target.session_id(), id.as_str());
    let mut handle = attach(target, input_id);
    handle.receipt.source_key = Some(id.to_string());
    handle.id = Some(id);
    handle
}

/// A handle on `run`; its cursor is the observation's current position.
pub(crate) fn run(target: SendTarget, run: TurnId) -> RunHandle {
    let cursor = target.current_cursor();
    RunHandle {
        target,
        run: run.into(),
        cursor,
        shared: Arc::new(HandleShared::pending()),
        operation_name: None,
        decode_error: Ok,
        output_type: std::marker::PhantomData,
    }
}

pub(crate) fn settled_output(input_id: InputId, outcome: SendOutcome) -> Result<TurnOutput> {
    match outcome {
        SendOutcome::Settled { output, .. } => Ok(*output),
        SendOutcome::Refused { refusal, .. } => Err(EmbedError::Runtime(*refusal)),
        outcome => Err(EmbedError::from(SendError::NotSettled {
            input_id,
            status: outcome.status(),
        })),
    }
}

// ---------------------------------------------------------------------------
// TurnEvents
// ---------------------------------------------------------------------------

/// The live activity of one run, as it is published on the session's
/// observation.
pub struct TurnEvents {
    pub(crate) inner: Pin<Box<dyn Stream<Item = Result<TurnActivity>> + Send>>,
}

impl TurnEvents {
    pub async fn next_activity(&mut self) -> Option<Result<TurnActivity>> {
        futures_util::StreamExt::next(self).await
    }
}

impl Stream for TurnEvents {
    type Item = Result<TurnActivity>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// What a [`cancel`](crate::LashSession::cancel) addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelTarget {
    /// An accepted input: withdrawn while queued, or its run cancelled once
    /// running.
    Input(InputId),
    /// A logical run.
    Run(TurnId),
}

/// Builder for one cancel (ADR 0039).
#[must_use = "a CancelBuilder does nothing until awaited"]
pub struct CancelBuilder {
    target: SendTarget,
    cancel: CancelTarget,
    request: cancel::CancelRequestSpec,
}

impl CancelBuilder {
    pub(crate) fn new(target: SendTarget, cancel: CancelTarget) -> Self {
        Self {
            target,
            cancel,
            request: cancel::CancelRequestSpec::default(),
        }
    }

    /// The cancel request's id; defaults to `cancel:{input|run}:{id}`.
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request.request_id = Some(id.into());
        self
    }

    /// Opaque host data Lash records without interpreting it.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.request.origin = Some(origin.into());
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.request.reason = Some(reason.into());
        self
    }

    pub fn mode(mut self, mode: TurnCancelMode) -> Self {
        self.request.mode = mode;
        self
    }

    pub fn undelivered(mut self, policy: TurnCancelUndeliveredInputPolicy) -> Self {
        self.request.undelivered = policy;
        self
    }

    async fn apply(self) -> Result<CancelReceipt> {
        let context = self.target.context().await?;
        cancel::apply(&context.parts, &self.cancel, self.request).await
    }
}

impl std::future::IntoFuture for CancelBuilder {
    type Output = Result<CancelReceipt>;
    type IntoFuture = BoxFuture<'static, Result<CancelReceipt>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.apply())
    }
}

/// What a cancel did.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CancelReceipt {
    /// An operation was withdrawn before the engine admitted its task.
    OperationWithdrawn {
        run: TurnId,
    },
    /// A host operation's durable cancellation signal accepted the request.
    OperationRequested {
        run: TurnId,
        request: lash_core::runtime::PluginTaskCancelRequest,
    },
    /// The input was still queued: its row is cancelled and no turn applied
    /// it. Its handle answers Cancelled with no output.
    Withdrawn(Box<PendingTurnInputCancelReceipt>),
    /// The input's run is running: a durable cancel request was placed on
    /// the run's cancellation gate.
    Requested {
        run: TurnId,
        receipt: Box<TurnCancelReceipt>,
    },
    /// The run already has a terminal (or the input was already applied and
    /// settled).
    AlreadySettled {
        run: TurnId,
    },
    NotFound,
}
