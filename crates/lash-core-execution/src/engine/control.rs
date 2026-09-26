//! The scope-close seam (FIG-3607 item 7, R10): what a logical root's end
//! and a session's close tell the owner of lifetime scopes.
//!
//! A root's `Turn(root)` scope, and a session's `Session` scope, own work
//! that outlives a single step (processes started under them, among others).
//! The drive calls this seam only after the end is durable: a root's terminal
//! evidence, or a session's `CloseSession` intent. It names no engine and no
//! registry: the process registry's scope-close adapter implements it, and
//! [`NoScopeClose`] stands in until one is installed.

use crate::store::{ControlIntentId, EnginePark, RootTerminal, StoreError};
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
    /// Whether this sink owns any scope. The drive records a root's close
    /// only for a sink that does: with no owner there is nothing to close,
    /// so no step is recorded and a root's journal ends at its commit.
    fn owns_scopes(&self) -> bool {
        true
    }

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
    fn owns_scopes(&self) -> bool {
        false
    }

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
