//! The scope-close seam (FIG-3607 item 7, R10): what a logical root's end
//! and a session's close tell the owner of lifetime scopes.
//!
//! A root's `Turn(root)` scope, and a session's `Session` scope, own work
//! that outlives a single step (processes started under them, among others).
//! The drive calls this seam only after the end is durable: a root's terminal
//! evidence, or a session's `CloseSession` intent. It names no engine and no
//! registry: the process registry's scope-close adapter implements it, and
//! [`NoScopeClose`] stands in until one is installed.

use crate::store::{ControlIntentId, RootTerminal, StoreError};
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
