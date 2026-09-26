//! One way in: [`LashSession::send`](crate::LashSession::send) accepts an
//! input durably and asks the session's engine to drive it (FIG-3600).
//!
//! The turn no longer runs in the caller's future. The caller holds a
//! [`SendHandle`] and reads what happened from what was recorded: the input's
//! root, then that root's terminal or its park. The engine is used only as a
//! wake barrier and to surface a drive it refused.
//!
//! Polling any one of a handle's [`events`](SendHandle::events),
//! [`outcome`](SendHandle::outcome) or [`output`](SendHandle::output) is
//! enough for the turn to complete, on every engine: on a core that runs no
//! engine and drives in the caller's task, each of them drives. The handle
//! remembers its answer, so a later call answers the same outcome without
//! driving again.

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

use crate::core::HeldWork;
use crate::durable_session::DurableSession;
use crate::error::{EmbedError, Result, SendError};
use crate::support::{
    EffectHost, LashSession, ProtocolTurnOptions, RuntimePersistence, TurnActivity,
    TurnActivitySink, TurnInput, TurnOutcome,
};
use crate::turn::{TurnCancelGuard, TurnOutput, TurnReport};

use lash_core::facade_support::{TurnCancelDisposition, TurnCancelMode, TurnCancelReceipt};
use lash_core::runtime::PendingTurnInputCancelReceipt;
use lash_core::store::{ParkId, ParkReason};

use follow::{Subject, Tap};
pub(crate) use mailbox::deposit_settled_root;

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
    pub(crate) store: Arc<dyn RuntimePersistence>,
    pub(crate) ops: DurableSessionOps,
    pub(crate) work: HeldWork,
    pub(crate) effect_host: Arc<dyn EffectHost>,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
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
        lash_core::store::load_persisted_session_state(self.parts.store.as_ref())
            .await
            .map_err(EmbedError::Store)?
            .map(|state| state.to_snapshot())
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

    /// The live replay position now: a cursor taken before an acceptance
    /// sees everything the acceptance's drive publishes.
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

    /// Register an in-flight send on the open session's cancel registry, so
    /// `cancel_running_turns*` reaches it (D1 §2.3).
    fn register(&self, parts: &SendParts, input: &InputId) -> Option<TurnCancelGuard> {
        match self {
            Self::Live(session) => Some(session.turn_cancels.register_send(parts, input)),
            Self::Durable(_) => None,
        }
    }
}

/// Refuse process-local turn context before anything is accepted: it cannot
/// survive durable acceptance (the same rule the remote boundary enforces).
fn refuse_live_turn_context(input: &TurnInput) -> Result<()> {
    let what = if input.protocol_extension.is_some() {
        Some("a live protocol turn extension")
    } else if input.turn_context.has_live_plugin_inputs() {
        Some("a live plugin turn input")
    } else if !input.turn_context.prompt_layer().is_empty() {
        Some("per-turn prompt")
    } else {
        None
    };
    match what {
        Some(what) => Err(EmbedError::from(SendError::LiveTurnContext { what })),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// SendBuilder
// ---------------------------------------------------------------------------

/// Builder for one [`send`](crate::LashSession::send).
///
/// Awaiting it commits the acceptance and asks the engine for a drive; it
/// yields a [`SendHandle`]. [`output`](Self::output) is the one-call form.
#[must_use = "a SendBuilder does nothing until awaited"]
pub struct SendBuilder {
    pub(crate) target: SendTarget,
    pub(crate) input: TurnInput,
    pub(crate) id: Option<TurnId>,
    pub(crate) ingress: TurnInputIngress,
    pub(crate) protocol_turn_options: Option<ProtocolTurnOptions>,
}

impl SendBuilder {
    pub(crate) fn new(target: SendTarget, input: TurnInput) -> Self {
        Self {
            target,
            input,
            id: None,
            ingress: TurnInputIngress::NextTurn,
            protocol_turn_options: None,
        }
    }

    /// The host's id for this input. It is the idempotency key **and** the
    /// root the input starts: it is stored verbatim as the row's source key,
    /// so the input's root is `TurnId(id)`.
    ///
    /// A retry validates the original submission digest, including after
    /// settlement. Identical content returns the original acceptance; changed
    /// content is refused.
    pub fn id(mut self, id: impl Into<TurnId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Where the input applies: [`TurnInputIngress::NextTurn`] (the default)
    /// or an active turn's checkpoint.
    pub fn ingress(mut self, ingress: TurnInputIngress) -> Self {
        self.ingress = ingress;
        self
    }

    pub fn protocol_turn_options(mut self, options: ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }

    /// Accept, then wait for the settled turn.
    pub async fn output(self) -> Result<TurnOutput> {
        self.await?.output().await
    }

    /// Accept, forward the root's live activity to `sink`, and answer its
    /// [`SendHandle::outcome_into`].
    pub async fn outcome_into(self, sink: &dyn TurnActivitySink) -> Result<SendOutcome> {
        self.await?.outcome_into(sink).await
    }

    /// Accept, forward the root's live activity to `sink`, then return the
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
            protocol_turn_options,
        } = self;
        refuse_live_turn_context(&input)?;
        let context = target.context().await?;
        if context.parts.work.refuses_sends() {
            return Err(EmbedError::from(SendError::NoSessionWork));
        }
        if let Some(options) = protocol_turn_options {
            input.protocol_turn_options = Some(options);
        }
        // The host id names the root; the drive runs the root's turns under
        // it, so the input carries no turn id of its own. An input sent
        // without one gets a fresh id, so its row is keyed and its root named
        // like any other.
        let host_id = id.or_else(|| input.trace_turn_id.take());
        input.trace_turn_id = None;
        let id = Some(host_id.unwrap_or_else(crate::turn::fresh_turn_id));
        let cursor = target.current_cursor();
        let enqueued = context
            .parts
            .ops
            .enqueue_turn_input(
                &context.parts.store,
                input,
                ingress,
                id.as_ref().map(ToString::to_string),
            )
            .await?;
        let receipt = TurnInputAcceptanceReceipt::from(&enqueued);
        let registration = target.register(&context.parts, &receipt.input_id);
        Ok(SendHandle {
            target,
            receipt,
            id,
            cursor,
            shared: Arc::new(HandleShared::pending(registration)),
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

/// What an input's root answered: the four-way status, the settled turn when
/// there is one, and the gaps in the live activity the follower observed.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SendOutcome {
    pub status: TurnStatus,
    /// The root that took the input: the one that answered it, or holds it
    /// parked. `None` only for an input withdrawn before any root took it.
    pub root: Option<TurnId>,
    /// `Some` for Answered, Failed, and Cancelled after the root ran; `None`
    /// for Parked and for an input withdrawn before it ran.
    pub output: Option<TurnOutput>,
    /// Where the follower's live activity is incomplete: the replay lost
    /// events, or the root ran where this process could not observe it (in
    /// another process, or before the handle's cursor). The collected
    /// [`TurnOutput::activities`] are then not the root's whole history; the
    /// report is still read from the store.
    pub gaps: Vec<lash_core::facade_support::LiveReplayGap>,
}

/// How an input's root stands once it stopped moving.
///
/// Parked is not terminal: the root holds its work until an operator
/// redrives, cancels or forks it, and a host re-awaits it through
/// [`LashSession::root`](crate::LashSession::root).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum TurnStatus {
    Answered,
    Failed,
    Cancelled,
    Parked(ParkedTurn),
}

/// A root that parked (ADR 0104 O3): durable and non-terminal.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParkedTurn {
    pub session_id: SessionId,
    pub root: TurnId,
    pub park_id: ParkId,
    pub reason: ParkReason,
    pub since_ms: u64,
    pub attempts: u32,
}

impl SendOutcome {
    /// This outcome for a transport: the four-way status, the report (with
    /// its collected activity) when the root ran, and the ids to re-attach
    /// by.
    pub fn to_remote(
        &self,
        session_id: &SessionId,
        input_id: &InputId,
    ) -> lash_remote_protocol::RemoteSendOutcome {
        let root_id = self.root.clone();
        let report = self.output.as_ref().map(|output| {
            let turn_id = root_id
                .clone()
                .unwrap_or_else(|| TurnId::from(input_id.as_str()));
            output
                .result
                .to_remote(session_id, &turn_id, &output.activities)
        });
        let status = match &self.status {
            TurnStatus::Answered => lash_remote_protocol::RemoteTurnStatus::Answered,
            TurnStatus::Failed => lash_remote_protocol::RemoteTurnStatus::Failed,
            TurnStatus::Cancelled => lash_remote_protocol::RemoteTurnStatus::Cancelled,
            TurnStatus::Parked(parked) => lash_remote_protocol::RemoteTurnStatus::Parked {
                root: parked.root.clone(),
                park_id: parked.park_id.feed_sequence(),
                reason: lash_remote_protocol::RemoteTurnParkReason::from(&parked.reason),
                since_ms: parked.since_ms,
                attempts: parked.attempts,
            },
        };
        lash_remote_protocol::RemoteSendOutcome {
            session_id: session_id.clone(),
            input_id: input_id.to_string(),
            root_id,
            status,
            report,
            gaps: self.gaps.iter().cloned().map(Into::into).collect(),
        }
    }
}

/// The status a committed outcome answers.
pub(crate) fn status_of_outcome(outcome: &TurnOutcome) -> TurnStatus {
    match outcome {
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. } => TurnStatus::Answered,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. }) => {
            TurnStatus::Cancelled
        }
        TurnOutcome::Stopped(_) => TurnStatus::Failed,
    }
}

// ---------------------------------------------------------------------------
// SendHandle and RootHandle
// ---------------------------------------------------------------------------

/// What every call on one handle shares: the answer once it is known, and
/// the handle's place in the session's cancel registry.
struct HandleShared {
    settled: Mutex<Option<SendOutcome>>,
    _registration: Option<TurnCancelGuard>,
}

impl HandleShared {
    fn pending(registration: Option<TurnCancelGuard>) -> Self {
        Self {
            settled: Mutex::new(None),
            _registration: registration,
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
        if let Some(output) = &outcome.output {
            for activity in &output.activities {
                tap.activity(activity).await;
            }
        }
        return Ok(outcome);
    }
    let context = target.context().await?;
    let mut from = follow::Position::at(cursor.clone());
    // A follow without a window answers; a pending one would go on from its
    // position.
    let outcome = loop {
        match Box::pin(follow::follow(&context, subject, from, &mut tap, None)).await? {
            follow::Followed::Answered(outcome) => break *outcome,
            follow::Followed::Pending(position) => from = position,
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

/// An accepted input: follow its live activity, and read how its root
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

    /// The receipt of the input's durable acceptance: the same receipt
    /// [`EnqueueTurnBuilder::send`](crate::EnqueueTurnBuilder::send) returns.
    pub fn receipt(&self) -> &TurnInputAcceptanceReceipt {
        &self.receipt
    }

    /// The host id, when the send set [`SendBuilder::id`].
    pub fn id(&self) -> Option<&TurnId> {
        self.id.as_ref()
    }

    /// Live activity of the root that applies this input, from the cursor
    /// taken before acceptance. Each call subscribes afresh from that cursor.
    /// It ends once the input's root settles or parks; draining it alone
    /// drives the turn to its answer, which the handle then remembers.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Input(self.receipt.clone()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// The input's root answer: resolves on a terminal **or** a park.
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

    /// [`outcome`](Self::outcome) narrowed to a settled turn. A parked root,
    /// or an input withdrawn before it ran, answers
    /// [`SendError::NotSettled`].
    pub async fn output(self) -> Result<TurnOutput> {
        let input_id = self.receipt.input_id.clone();
        settled_output(input_id, self.outcome().await?)
    }

    /// Forward the root's live activity to `sink` as it arrives, and answer
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
    /// running root.
    pub fn cancel(&self) -> CancelBuilder {
        CancelBuilder::new(
            self.target.clone(),
            CancelTarget::Input(self.receipt.input_id.clone()),
        )
    }
}

/// A logical root, re-awaited by id: after a restart, a park verb, or from a
/// handle that only knows the host id.
pub struct RootHandle {
    target: SendTarget,
    root: TurnId,
    cursor: lash_core::SessionCursor,
    shared: Arc<HandleShared>,
}

impl RootHandle {
    pub fn root(&self) -> &TurnId {
        &self.root
    }

    /// Live activity of the root from the moment this handle was made; no
    /// earlier activity is replayed.
    pub fn events(&self) -> TurnEvents {
        spawn_events(
            self.target.clone(),
            Subject::Root(self.root.clone()),
            self.cursor.clone(),
            Arc::clone(&self.shared),
        )
    }

    /// How the root answers. A root still parked answers Parked again.
    pub async fn outcome(self) -> Result<SendOutcome> {
        settle(
            &self.target,
            &Subject::Root(self.root.clone()),
            &self.cursor,
            &self.shared,
            Tap::Quiet,
        )
        .await
    }

    pub async fn output(self) -> Result<TurnOutput> {
        let root = self.root.clone();
        let outcome = self.outcome().await?;
        settled_output(InputId::from(root.as_str()), outcome)
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
        shared: Arc::new(HandleShared::pending(None)),
    }
}

/// A handle on the input a send accepted under host id `id`: a keyed input's
/// id is derived from its session and key, so no read finds it. An id that
/// was never accepted answers like a withdrawn input.
pub(crate) fn attach_id(target: SendTarget, id: TurnId) -> SendHandle {
    let input_id = InputId::from(lash_core::PendingTurnInputDraft::keyed_input_id(
        &target.session_id(),
        id.as_str(),
    ));
    let mut handle = attach(target, input_id);
    handle.receipt.source_key = Some(id.to_string());
    handle.id = Some(id);
    handle
}

/// A handle on `root`; its cursor is the observation's current position.
pub(crate) fn root(target: SendTarget, root: TurnId) -> RootHandle {
    let cursor = target.current_cursor();
    RootHandle {
        target,
        root,
        cursor,
        shared: Arc::new(HandleShared::pending(None)),
    }
}

pub(crate) fn settled_output(input_id: InputId, outcome: SendOutcome) -> Result<TurnOutput> {
    match outcome.output {
        Some(output) => Ok(output),
        None => Err(EmbedError::from(SendError::NotSettled {
            input_id,
            status: outcome.status,
        })),
    }
}

// ---------------------------------------------------------------------------
// TurnEvents
// ---------------------------------------------------------------------------

/// The live activity of one root, as it is published on the session's
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
    /// An accepted input: withdrawn while queued, or its root cancelled once
    /// running.
    Input(InputId),
    /// A logical root.
    Root(TurnId),
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

    /// The cancel request's id; defaults to `cancel:{input|root}:{id}`.
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

    pub fn undelivered(mut self, disposition: TurnCancelDisposition) -> Self {
        self.request.undelivered = disposition;
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
    /// The input was still queued: its row is cancelled and no turn applied
    /// it. Its handle answers Cancelled with no output.
    Withdrawn(Box<PendingTurnInputCancelReceipt>),
    /// The input's root is running: a durable cancel request was placed on
    /// the root's cancellation gate.
    Requested {
        root: TurnId,
        receipt: Box<TurnCancelReceipt>,
    },
    /// The root already has a terminal (or the input was already applied and
    /// settled).
    AlreadySettled {
        root: TurnId,
    },
    NotFound,
}

/// Cancel an in-flight send from the session's cancel registry: the
/// process-local `cancel_running_turns*` lever (D1 §2.3), spawned so the
/// lever stays synchronous.
pub(crate) fn spawn_registry_cancel(
    parts: SendParts,
    input: InputId,
    origin: Option<String>,
    mode: TurnCancelMode,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            session_id = %parts.session_id,
            input_id = %input,
            "cancel_running_turns outside a Tokio runtime cannot reach a sent input"
        );
        return;
    };
    runtime.spawn(async move {
        let request = cancel::CancelRequestSpec {
            origin,
            mode,
            ..Default::default()
        };
        if let Err(error) =
            cancel::apply(&parts, &CancelTarget::Input(input.clone()), request).await
        {
            tracing::warn!(
                session_id = %parts.session_id,
                input_id = %input,
                error = %error,
                "cancel_running_turns could not cancel a sent input"
            );
        }
    });
}
