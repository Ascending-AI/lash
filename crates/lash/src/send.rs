//! One way in: [`LashSession::send`](crate::LashSession::send) accepts an
//! input durably and asks the session's engine to drive it (FIG-3600).
//!
//! The turn no longer runs in the caller's future. The caller holds a
//! [`SendHandle`] and reads what happened from what was recorded: the input's
//! root, then that root's terminal or its park. The engine is used only as a
//! wake barrier and to surface a drive it refused.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::future::BoxFuture;
use lash_core::runtime::{TurnInputAcceptanceReceipt, TurnInputIngress};
use lash_core::{InputId, SessionId, TurnId};

use crate::durable_session::DurableSession;
use crate::error::{EmbedError, Result, SendError};
use crate::support::{
    LashSession, ProtocolTurnOptions, TurnActivity, TurnActivitySink, TurnInput, TurnOutcome,
};
use crate::turn::{TurnOutput, TurnReport};

use lash_core::facade_support::{TurnCancelDisposition, TurnCancelMode, TurnCancelReceipt};
use lash_core::runtime::PendingTurnInputCancelReceipt;
use lash_core::store::{ParkId, ParkReason};

/// The session a send, a handle or a cancel is bound to.
#[derive(Clone)]
pub(crate) enum SendTarget {
    /// An open session: answers also bring its resident runtime to the
    /// committed head.
    Live(LashSession),
    /// A Durable Session: no runtime to refresh.
    Durable(DurableSession),
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
    /// A send whose root already has terminal evidence commits nothing and
    /// answers from that evidence.
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

    /// Accept, forward the turn's live activity to `sink`, then return the
    /// settled report.
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnReport> {
        self.await?.output_into(sink).await
    }

    async fn accept(self) -> Result<SendHandle> {
        todo!("S5b: acceptance")
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

/// What an input's root answered: the four-way status, and the settled turn
/// when there is one.
#[derive(Clone, Debug)]
pub struct SendOutcome {
    pub status: TurnStatus,
    /// `Some` for Answered, Failed, and Cancelled after the root ran; `None`
    /// for Parked and for an input withdrawn before it ran.
    pub output: Option<TurnOutput>,
}

/// How an input's root stands once it stopped moving.
///
/// Parked is not terminal: the root holds its work until an operator
/// redrives, cancels or forks it, and a host re-awaits it through
/// [`LashSession::root`](crate::LashSession::root).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TurnStatus {
    Answered,
    Failed,
    Cancelled,
    Parked(ParkedTurn),
}

/// A root that parked (ADR 0104 O3): durable and non-terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedTurn {
    pub session_id: SessionId,
    pub root: TurnId,
    pub park_id: ParkId,
    pub reason: ParkReason,
    pub since_ms: u64,
    pub attempts: u32,
}

/// The status a committed outcome answers.
pub(crate) fn status_of_outcome(outcome: &TurnOutcome) -> TurnStatus {
    match outcome {
        TurnOutcome::Finished(_) | TurnOutcome::AgentFrameSwitch { .. } => TurnStatus::Answered,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. }) => {
            TurnStatus::Cancelled
        }
        TurnOutcome::Stopped(_) | TurnOutcome::Queued { .. } => TurnStatus::Failed,
    }
}

// ---------------------------------------------------------------------------
// SendHandle and RootHandle
// ---------------------------------------------------------------------------

/// An accepted input: follow its live activity, and read how its root
/// answered.
///
/// Dropping a handle stops nothing.
pub struct SendHandle {
    pub(crate) target: SendTarget,
    pub(crate) receipt: TurnInputAcceptanceReceipt,
    pub(crate) id: Option<TurnId>,
    pub(crate) cursor: lash_core::SessionCursor,
    pub(crate) settled: Option<Box<SendOutcome>>,
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
    /// It ends once the input's root settles or parks.
    pub fn events(&self) -> TurnEvents {
        todo!("S5b: events")
    }

    /// The input's root answer: resolves on a terminal **or** a park.
    pub async fn outcome(self) -> Result<SendOutcome> {
        todo!("S5b: outcome")
    }

    /// [`outcome`](Self::outcome) narrowed to a settled turn. A parked root,
    /// or an input withdrawn before it ran, answers
    /// [`SendError::NotSettled`].
    pub async fn output(self) -> Result<TurnOutput> {
        let input_id = self.receipt.input_id.clone();
        settled_output(input_id, self.outcome().await?)
    }

    /// Forward [`events`](Self::events) to `sink` as they arrive, then
    /// [`output`](Self::output), returning its report.
    pub async fn output_into(self, sink: &dyn TurnActivitySink) -> Result<TurnReport> {
        let _ = sink;
        todo!("S5b: output_into")
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
    pub(crate) target: SendTarget,
    pub(crate) root: TurnId,
    pub(crate) cursor: lash_core::SessionCursor,
}

impl RootHandle {
    pub fn root(&self) -> &TurnId {
        &self.root
    }

    /// Live activity of the root from the moment this handle was made; no
    /// earlier activity is replayed.
    pub fn events(&self) -> TurnEvents {
        todo!("S5b: root events")
    }

    /// How the root answers. A root still parked answers Parked again.
    pub async fn outcome(self) -> Result<SendOutcome> {
        todo!("S5b: root outcome")
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
    let _ = (target, input_id);
    todo!("S5b: attach")
}

/// A handle on `root`; its cursor is the observation's current position.
pub(crate) fn root(target: SendTarget, root: TurnId) -> RootHandle {
    let _ = (target, root);
    todo!("S5b: root")
}

fn settled_output(input_id: InputId, outcome: SendOutcome) -> Result<TurnOutput> {
    match outcome.output {
        Some(output) => Ok(output),
        None => Err(EmbedError::Send(SendError::NotSettled {
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
    pub(crate) target: SendTarget,
    pub(crate) cancel: CancelTarget,
    pub(crate) request_id: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) mode: TurnCancelMode,
    pub(crate) undelivered: TurnCancelDisposition,
}

impl CancelBuilder {
    pub(crate) fn new(target: SendTarget, cancel: CancelTarget) -> Self {
        Self {
            target,
            cancel,
            request_id: None,
            origin: None,
            reason: None,
            mode: TurnCancelMode::Immediate,
            undelivered: TurnCancelDisposition::Defer,
        }
    }

    /// The cancel request's id; defaults to `cancel:{input|root}:{id}`.
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    /// Opaque host data Lash records without interpreting it.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn mode(mut self, mode: TurnCancelMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn undelivered(mut self, disposition: TurnCancelDisposition) -> Self {
        self.undelivered = disposition;
        self
    }

    async fn apply(self) -> Result<CancelReceipt> {
        todo!("S5b: cancel")
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
    Withdrawn(PendingTurnInputCancelReceipt),
    /// The input's root is running: a durable cancel request was placed on
    /// the root's cancellation gate.
    Requested {
        root: TurnId,
        receipt: TurnCancelReceipt,
    },
    /// The root already has a terminal (or the input was already applied and
    /// settled).
    AlreadySettled {
        root: TurnId,
    },
    NotFound,
}
