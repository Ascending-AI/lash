use crate::core_internal::await_event_scope_not_retirable;
use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome,
    RuntimeError, SessionId,
};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Result of preparing an externally routable tool completion key.
pub enum CompletionKeyPreparation {
    NotNeeded,
    Unsupported,
    Issued(AwaitEventKey),
}

/// Shared AwaitEvent contract for effect boundaries.
///
/// Both the deployment-level [`EffectHost`] factory and the per-run
/// [`RuntimeEffectController`] resolve AwaitEvents.
#[async_trait::async_trait]
pub trait AwaitEventResolver: Send + Sync {
    /// Stable identity of the durable authority that minted keys accepted by
    /// this resolver. Durable turn-control composition uses this to prevent a
    /// host label from being paired with another owner's controller and keys.
    ///
    /// Every resolver answers explicitly: turn control and durable tool-child
    /// completion refuse a resolver that names no authority, so a forwarding
    /// layer must pass its inner answer through rather than inherit `None`.
    fn await_event_authority_binding_id(&self) -> Option<String>;

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let _ = (scope, wait);
        if may_defer {
            Ok(CompletionKeyPreparation::Unsupported)
        } else {
            Ok(CompletionKeyPreparation::NotNeeded)
        }
    }

    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event keys",
        ))
    }

    async fn resolve_await_event(
        &self,
        _key: &AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        Ok(ResolveOutcome::UnknownOrRevoked)
    }

    /// [`resolve_await_event`](Self::resolve_await_event) for a write nobody
    /// waits on, such as a committed turn's terminal publication (FIG-3978).
    /// A resolver that can hand the write to its engine durably returns
    /// `None` without waiting for the outcome; the default resolves in place
    /// and answers the outcome.
    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.resolve_await_event(key, resolution).await.map(Some)
    }

    /// Turn owners use this as a synchronous start gate before beginning a
    /// new effect. Durable owners must perform that read through their
    /// handler-scoped, replay-aware controller: its result affects subsequent
    /// command order and therefore must replay identically after an owner
    /// crash. An unresolved promise returns `None` and remains open.
    async fn peek_await_event(
        &self,
        _key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event reads",
        ))
    }

    async fn await_await_event(
        &self,
        _key: &AwaitEventKey,
        _cancel: CancellationToken,
        _deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event waits",
        ))
    }

    async fn revoke_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support revoking await-event waits",
        ))
    }

    /// Cancel every *outstanding* durable wait for `session_id` without
    /// deleting the session: each waiter receives a terminal
    /// [`Resolution::Cancelled`] instead of hanging, late resolves observe
    /// that terminal, and waits registered afterwards behave normally — in
    /// contrast to [`revoke_await_events_for_session`](Self::revoke_await_events_for_session),
    /// which tombstones the session's waits forever.
    ///
    /// The default errors loudly: an effect boundary that tracks durable waits
    /// must implement this to honor the host lever, and one that cannot must
    /// not silently claim success.
    async fn cancel_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventCancelUnsupported,
            "this effect boundary does not support cancelling durable waits",
        ))
    }

    /// Drop every promise of the terminal non-session `scope` and fence the
    /// scope permanently: later mints, resolves, peeks, and waits under it
    /// report `await_event_unknown_or_revoked`, including after a restart on
    /// durable hosts. Session-bearing scopes are refused with
    /// `await_event_scope_not_retirable`; they are revoked as a family through
    /// [`revoke_await_events_for_session`](Self::revoke_await_events_for_session).
    ///
    /// This is the promise half of [`EffectHost::retire_effect_journal`]: a
    /// durable host performs both halves in one transaction there and answers
    /// this lever from the same code path.
    async fn retire_await_events_for_scope(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support retiring await-event scopes",
        ))
    }

    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// only when no waiter is parked on a promise under `scope`, answering
    /// whether it retired: `Ok(false)` leaves the scope untouched and unfenced.
    /// The proof and the fence must land under one lock. Resolvers that prove
    /// quiescence elsewhere (a durable journal reads its wait rows in the
    /// retirement transaction) retire unconditionally here and answer `true`.
    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.retire_await_events_for_scope(scope)
            .await
            .map(|()| true)
    }

    /// Lift the fence
    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// left on a non-session `scope`, because its owner is registered again.
    /// Resolvers that never fence have nothing to lift and answer `Ok`.
    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Err(await_event_scope_not_retirable(scope));
        }
        Ok(())
    }

    /// Whether `scope` is fenced by a scope-exact retirement this resolver
    /// holds. Every admission path that runs effects for a scope consults it
    /// before executing, so a retired scope is refused even on a path that
    /// makes no journal claim. Resolvers whose
    /// journal already refuses retired scopes at claim time may answer
    /// `false`.
    async fn await_event_scope_is_retired(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}
