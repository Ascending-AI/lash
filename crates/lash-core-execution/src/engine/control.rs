//! The scope-close seam (FIG-3607 item 7, R10): what a logical root's end
//! and a session's close tell the owner of lifetime scopes.
//!
//! A root's `Turn(root)` scope, and a session's `Session` scope, own work
//! that outlives a single step (processes started under them, among others).
//! The drive calls this seam only after the end is durable: a root's terminal
//! evidence, or a session's `CloseSession` intent. It names no engine and no
//! registry: the process registry's scope-close adapter implements it, and
//! [`NoScopeClose`] stands in until one is installed.

use crate::store::{ControlIntentId, EnginePark, ParkId, ParkReason, RootTerminal, StoreError};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};

/// Where the drive reports a closed root scope or session scope.
///
/// Guarantees a caller of this trait keeps:
///
/// - it is called only after the root's terminal evidence (or the session's
///   close intent) is durable;
/// - it is called at least once per terminal root: a crash between the
///   evidence and the call re-runs the recorded step that calls it;
/// - it is never called for a parked root, which holds its scope open.
///
/// An implementor must be idempotent per `(session, root)` and per intent.
#[async_trait::async_trait]
pub trait ScopeCloseSink: Send + Sync {
    /// Close `Turn(root)` after its terminal evidence.
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), StoreError>;

    /// Close `Session(session)` and the listed roots after its
    /// `CloseSession` intent.
    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError>;
}

/// The sink of a host that installed no scope owner: closing is a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoScopeClose;

#[async_trait::async_trait]
impl ScopeCloseSink for NoScopeClose {
    async fn close_root_scope(&self, _terminal: &RootTerminal) -> Result<(), StoreError> {
        Ok(())
    }

    async fn close_session_scope(
        &self,
        _session: &SessionId,
        _intent: ControlIntentId,
        _roots: &[TurnId],
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

/// The replay key of a session's `BeginSessionClose` step, inside its
/// `SessionDelete` scope: one close per session, so every retry of the
/// deletion replays the recorded close.
#[must_use]
pub fn begin_session_close_replay_key(session: &SessionId) -> String {
    format!("{}:begin-close", session.as_str())
}

/// One logical root, as an engine's control verbs address it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootRef {
    pub session: SessionId,
    pub root: TurnId,
}

/// What an engine did for a control verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineAck {
    /// The engine resumed the execution holding the root.
    Resumed,
    /// The engine stopped the root's execution for good.
    Released,
    /// The engine held no execution for the root: nothing to do.
    NothingHeld,
}

/// Why an engine could not carry out a control verb. A retryable refusal is
/// retained on the intent for reconciliation to re-apply; a permanent one is
/// retained as failed, visible to an operator.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineRefusal {
    #[error("{0}")]
    Retryable(String),
    #[error("{code}: {message}")]
    Permanent {
        code: crate::RuntimeErrorCode,
        message: String,
    },
}

/// The engine half of the control verbs (ADR 0104 O3/O4, FIG-3600 S7): what
/// an engine does to its executions after the store recorded an intent. It
/// names no engine; each engine implements it over its own executions.
///
/// Every method is idempotent: reconciliation re-applies an intent whose
/// acknowledgement a crash lost.
#[async_trait::async_trait]
pub trait SessionControlEngine: Send + Sync {
    /// O3: the engine's stalled work becomes lash parks. Every execution the
    /// engine stopped retrying is recorded through `parks` with
    /// [`ParkReason::EngineRetryExhausted`] and the engine's handle; one
    /// whose target already ended is released instead. A stalled
    /// admission-only drive (no root, no effects) is resumed, never parked.
    ///
    /// Bounded: at most `page.limit` executions after `page.after`, in the
    /// engine's own order; the report's `next` resumes the listing, and
    /// `None` means it ran out. Idempotent: a second pass over the same
    /// stalled execution records nothing new.
    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal>;

    /// O4 redrive: resume the execution holding the root's park. An engine
    /// holding none answers [`EngineAck::NothingHeld`], and the caller
    /// schedules a drive instead.
    async fn resume_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal>;

    /// O4 release: stop the root's execution for good, AFTER the store
    /// recorded the root's terminal evidence. Never proof of a lash outcome
    /// (ADR 0104 O4): the evidence is the store's.
    async fn release_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal>;
}

/// The control engine of an engine that holds no executions across calls
/// (the interim native engine): there is nothing to release.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEngineControl;

#[async_trait::async_trait]
impl SessionControlEngine for NoEngineControl {
    async fn reconcile_parks(
        &self,
        _parks: &dyn ParkRecoveryWriter,
        _page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        Ok(ParkReconcileReport::default())
    }

    async fn resume_root(
        &self,
        _target: &RootRef,
        _engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }

    async fn release_root(
        &self,
        _target: &RootRef,
        _engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }
}

/// An engine's opaque position in its own listing of stalled executions:
/// what [`EnginePage::after`] resumes from. Meaningful only to the engine
/// that issued it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineCursor(String);

impl EngineCursor {
    /// The engine's position `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The position as the engine wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded page of an engine's stalled-work listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnginePage {
    /// Resume strictly after this position; `None` starts at the beginning.
    pub after: Option<EngineCursor>,
    /// Read at most this many executions.
    pub limit: NonZeroUsize,
}

/// The work a park holds, as the engine names it to the park writer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParkTarget {
    /// A session's logical root.
    Root { session: SessionId, root: TurnId },
    /// A process.
    Process { process: crate::ProcessId },
}

/// What [`ParkRecoveryWriter::record_engine_park`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineParkRecorded {
    /// A new park, with reason `EngineRetryExhausted`.
    Parked(ParkId),
    /// The target already held a park: it keeps its reason and now carries
    /// the engine's handle. A park that already carried the same handle is
    /// left as it is.
    AttachedToExisting(ParkId),
    /// The target already has terminal evidence: the engine should release
    /// its execution.
    TargetTerminal,
    /// The target is gone (its session deleted, or the process pruned): the
    /// engine should release its execution.
    TargetGone,
}

/// The park writer an engine's reconcile records stalled work through: the
/// recovery path for a park the execution could not record itself, because
/// the engine stopped running it before it reached its own park step.
/// Reconciliation is its only caller (ADR 0105 §9).
///
/// Its writes converge with the execution's own park write: both key the
/// park by its target, so a divergence park the execution recorded first
/// keeps its reason and gains the engine's handle.
#[async_trait::async_trait]
pub trait ParkRecoveryWriter: Send + Sync {
    /// Park `target` for `reason`, carrying the engine's `engine` handle.
    async fn record_engine_park(
        &self,
        target: &ParkTarget,
        reason: ParkReason,
        engine: EnginePark,
    ) -> Result<EngineParkRecorded, StoreError>;
}

/// What one [`SessionControlEngine::reconcile_parks`] pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParkReconcileReport {
    /// Targets this pass parked.
    pub parked: Vec<ParkTarget>,
    /// Stalled executions whose target already held a park.
    pub attached: usize,
    /// Roots whose execution this pass released because the store had
    /// already ended them.
    pub released: Vec<RootRef>,
    /// Sessions whose stalled admission-only drive this pass resumed.
    pub resumed_drives: Vec<SessionId>,
    /// Stalled executions this pass left as they were.
    pub unchanged: usize,
    /// Where the next pass resumes; `None` when this one read to the end.
    pub next: Option<EngineCursor>,
}
