//! The controller's reads of a session's revocation and a turn's cancellation
//! gate, and the one-way resolve that publishes a committed turn's terminal
//! (FIG-3978).

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, Resolution, ResolveOutcome, RuntimeError,
    RuntimeErrorCode,
};
use lash_sansio::SessionId;

use super::{RestateControllerContext, RestateRuntimeEffectController};
use crate::durable_wait::{
    RestateDurableWaitResolveRequest, RestateTurnGatePeek,
    restate_await_event_key_is_valid_for_authority, restate_unknown_or_revoked,
};

/// A turn's cancellation gate or its escalation: the waits whose every write
/// goes through their session index's `resolve`.
pub(super) fn is_turn_cancel_gate(key: &AwaitEventKey) -> bool {
    matches!(
        key.wait,
        AwaitEventWaitIdentity::TurnCancelGate | AwaitEventWaitIdentity::TurnCancelEscalation
    )
}

fn engine_error(err: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::EngineEffectController, err.to_string())
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    pub(super) async fn require_active_session(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<(), RuntimeError> {
        if let Some(session_id) = session_id
            && self
                .context
                .session_is_revoked(&self.namespace, SessionId::from(session_id.to_string()))
                .await
                .map_err(engine_error)?
        {
            return Err(restate_unknown_or_revoked());
        }
        Ok(())
    }

    /// The turn-control peek a `PeekAwaitEvent` effect journals. The effect
    /// peeks only the running turn's own gate, whose root is still open, so
    /// the index's copy is the whole answer. Any other key keeps the general
    /// read.
    pub(super) async fn peek_turn_gate(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        use lash_core::AwaitEventResolver as _;
        if !is_turn_cancel_gate(key) {
            return self.peek_await_event(key).await;
        }
        self.mirrored_turn_gate(key).await
    }

    /// The terminal a gate's session index holds, from one shared read that
    /// answers the revocation too. The general peek of a gate reads the
    /// workflow on `None`: it may be asking after a retired root's gate,
    /// whose terminal only the workflow still holds.
    pub(super) async fn mirrored_turn_gate(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        match self
            .context
            .peek_turn_gate(&self.namespace, key.clone())
            .await
            .map_err(engine_error)?
        {
            RestateTurnGatePeek::Revoked => Err(restate_unknown_or_revoked()),
            RestateTurnGatePeek::Open(resolution) => Ok(resolution),
        }
    }

    /// A one-way send to the key's index: the resolve is journaled as the
    /// send and runs in the index's own invocation, so the caller waits for
    /// neither the index nor the workflow write behind it.
    pub(super) async fn publish_resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Ok(Some(ResolveOutcome::UnknownOrRevoked));
        }
        self.context
            .publish_event(
                &self.namespace,
                RestateDurableWaitResolveRequest {
                    key: key.clone(),
                    resolution,
                },
            )
            .await
            .map_err(engine_error)?;
        Ok(None)
    }
}
