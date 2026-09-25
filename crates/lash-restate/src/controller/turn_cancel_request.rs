//! The durable-wait request each wait's turn-cancel gate race registers.

use lash_core::{
    AwaitEventWaitIdentity, ExecutionScope, RuntimeEffectControllerError, RuntimeEffectInvocation,
    RuntimeErrorCode,
};

use crate::RestateAuthorityId;
use crate::durable_wait::{
    RestateDurableWaitAwaitRequest, restate_await_event_key_for_authority,
    restate_durable_wait_request,
};

fn restate_turn_cancel_wait_request(
    authority_id: &RestateAuthorityId,
    invocation: &RuntimeEffectInvocation,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    let Some(turn_id) = invocation.attribution.turn_id.as_ref() else {
        return Ok(None);
    };
    let Some(scope) = turn_cancel_scope else {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMissing,
            "turn effects that observe cancellation require a durable turn-cancel scope",
        ));
    };
    if matches!(scope, ExecutionScope::Process { .. }) {
        return Ok(None);
    }
    let scope @ ExecutionScope::Turn { .. } = scope else {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMismatch,
            "turn-cancel scope must be a matching turn scope or an explicit process scope",
        ));
    };
    scope
        .validate()
        .map_err(RuntimeEffectControllerError::from)?;
    if scope.session_id() != invocation.attribution.session_id.as_ref()
        || scope.turn_id() != Some(turn_id)
    {
        return Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RestateTurnCancelScopeMismatch,
            "turn-cancel scope must match the runtime effect invocation",
        ));
    }
    restate_turn_cancel_gate_request(authority_id, scope).map(Some)
}

/// The gate request an effect-group wait races, from the wait's observed
/// turn scope (FIG-3672 P9). A group wait carries no invocation to cross-check,
/// so only a turn scope names a gate; a process scope, or none, races nothing.
pub(super) fn restate_group_turn_cancel_wait_request(
    authority_id: &RestateAuthorityId,
    turn_cancel: &lash_core::TurnCancelWait,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    let Some(scope @ ExecutionScope::Turn { .. }) = turn_cancel.observed_scope() else {
        return Ok(None);
    };
    scope
        .validate()
        .map_err(RuntimeEffectControllerError::from)?;
    restate_turn_cancel_gate_request(authority_id, scope).map(Some)
}

/// The durable-wait request for the turn-cancel gate of a validated turn
/// scope: what every wait that races the gate registers against.
fn restate_turn_cancel_gate_request(
    authority_id: &RestateAuthorityId,
    scope: &ExecutionScope,
) -> Result<RestateDurableWaitAwaitRequest, RuntimeEffectControllerError> {
    let key = restate_await_event_key_for_authority(
        authority_id,
        scope,
        AwaitEventWaitIdentity::TurnCancelGate,
    )?;
    Ok(restate_durable_wait_request(
        &key,
        None,
        &lash_core::facade_support::SystemClock,
    ))
}

pub(crate) fn restate_timer_turn_cancel_wait_request(
    authority_id: &RestateAuthorityId,
    invocation: &RuntimeEffectInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(authority_id, invocation, turn_cancel_scope)
}

pub(super) fn restate_process_turn_cancel_wait_request(
    authority_id: &RestateAuthorityId,
    invocation: &RuntimeEffectInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(authority_id, invocation, turn_cancel_scope)
}

pub(crate) fn restate_await_event_turn_cancel_wait_request(
    authority_id: &RestateAuthorityId,
    invocation: &RuntimeEffectInvocation,
    observe_turn_cancel: bool,
    turn_cancel_scope: Option<&ExecutionScope>,
) -> Result<Option<RestateDurableWaitAwaitRequest>, RuntimeEffectControllerError> {
    if !observe_turn_cancel {
        return Ok(None);
    }
    restate_turn_cancel_wait_request(authority_id, invocation, turn_cancel_scope)
}
