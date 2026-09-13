use super::*;

/// What the test gate race does after a wake lands while the guarded wait is
/// still pending. Mirrors the deployed `race_turn_cancel_gate` flow: a deferred
/// stop re-registers on the escalation key and keeps waiting; anything else
/// unwinds.
pub(super) enum TestTurnCancelWakeStep {
    Continue(TestTurnCancelRegistration),
    Unwind(RestateTurnCancelWake),
}

pub(super) fn test_turn_cancel_wake_step(
    gate: &TestTurnCancelGate,
    turn_cancel_key: &AwaitEventKey,
    escalated: bool,
    wake: RestateTurnCancelWake,
) -> Result<TestTurnCancelWakeStep, TerminalError> {
    if escalated || wake != RestateTurnCancelWake::TurnCancelDeferred {
        return Ok(TestTurnCancelWakeStep::Unwind(wake));
    }
    let escalation_key = match crate::durable_wait::restate_authority_id_for_key(turn_cancel_key) {
        Some(authority) => crate::durable_wait::restate_await_event_key_for_authority(
            &authority,
            &turn_cancel_key.scope,
            AwaitEventWaitIdentity::TurnCancelEscalation,
        ),
        None => restate_await_event_key(
            &turn_cancel_key.scope,
            AwaitEventWaitIdentity::TurnCancelEscalation,
        ),
    }
    .map_err(TerminalError::from_error)?;
    match gate.register(escalation_key)? {
        TestTurnCancelRegistrationVerdict::Registered(registration) => {
            Ok(TestTurnCancelWakeStep::Continue(registration))
        }
        TestTurnCancelRegistrationVerdict::Revoked => Ok(TestTurnCancelWakeStep::Unwind(
            RestateTurnCancelWake::SessionRevoked,
        )),
    }
}

pub(in crate::tests) fn runtime_invocation(
    kind: RuntimeEffectKind,
    effect_id: &str,
) -> lash_core::RuntimeEffectInvocation {
    lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            durable_turn_scope("session", "turn"),
            format!("session:turn:1:0:{}:{effect_id}", kind.as_str()),
        )
        .expect("valid recording-context effect address"),
        lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 0),
        effect_id,
    )
}

pub(in crate::tests) fn test_turn_cancel_wait_request(
    session_id: &SessionId,
    turn_id: &TurnId,
) -> RestateDurableWaitAwaitRequest {
    let key = restate_await_event_key(
        &durable_turn_scope(session_id, turn_id),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("test turn cancellation gate key");
    RestateDurableWaitAwaitRequest {
        key,
        deadline: None,
    }
}
