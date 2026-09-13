use super::*;

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
