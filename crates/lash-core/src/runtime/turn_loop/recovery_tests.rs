use super::commit::recovered_turn_cancel_closure;
use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, TurnAddress,
    TurnCancelClosureAuthorization, TurnCancelClosureProposal, TurnCancelIntentSnapshot,
};

fn closure_authorization(
    turn_id: &str,
    binding_id: &str,
    admitted_scope: ExecutionScope,
    fencing_token: u64,
) -> TurnCancelClosureAuthorization {
    let address = TurnAddress::new("recovery-session", turn_id);
    let turn_scope = address.execution_scope();
    let key = |wait, name: &str| AwaitEventKey {
        scope: turn_scope.clone(),
        wait,
        key_id: format!("{turn_id}:{name}"),
        signature: format!("signature:{turn_id}:{name}"),
    };
    TurnCancelClosureAuthorization::new(
        address,
        binding_id,
        admitted_scope,
        key(AwaitEventWaitIdentity::TurnCancelGate, "cancel"),
        key(AwaitEventWaitIdentity::TurnCancelEscalation, "escalation"),
        key(AwaitEventWaitIdentity::TurnTerminal, "terminal"),
        TurnCancelClosureProposal::CompletionSealed,
        TurnCancelIntentSnapshot::Absent,
        &crate::store_backend_support::sealed_drive_fence(
            "recovery-session".into(),
            fencing_token,
            crate::store::AdmissionId::new("old-admission"),
        ),
    )
    .expect("valid closure authorization")
}

#[test]
fn recovered_commit_adopts_the_exact_predecessor_authorization() {
    let binding_id = "binding";
    let scope = ExecutionScope::turn("recovery-session", "root-turn");
    let preserved = closure_authorization("root-turn", binding_id, scope.clone(), 7);
    let unrelated = closure_authorization(
        "unrelated-turn",
        binding_id,
        ExecutionScope::turn("recovery-session", "unrelated-turn"),
        7,
    );

    let adopted = recovered_turn_cancel_closure(
        vec![unrelated, preserved.clone()],
        &TurnAddress::new("recovery-session", "root-turn"),
        binding_id,
        &scope,
    )
    .expect("valid pending closure")
    .expect("recovered turn closure");

    assert_eq!(adopted, preserved);
    assert_eq!(adopted.authorizing_fencing_token(), 7);
}

#[test]
fn recovered_commit_rejects_changed_admission_scope() {
    let original_scope = ExecutionScope::process(crate::ProcessId::fixture("original-process"));
    let binding_id = crate::turn_control_binding_id_for_scope("binding", &original_scope)
        .expect("physical binding id");
    let preserved = closure_authorization("root-turn", &binding_id, original_scope, 7);
    let error = recovered_turn_cancel_closure(
        vec![preserved],
        &TurnAddress::new("recovery-session", "root-turn"),
        &binding_id,
        &ExecutionScope::process(crate::ProcessId::fixture("successor-process")),
    )
    .expect_err("scope drift must fail closed");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::InvalidTurnCancelRequest
    );
}
