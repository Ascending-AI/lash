use super::commit::recovered_turn_cancel_closure;
use super::lease::is_resumable_turn_or_follow_on;
use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, LeaseOwnerIdentity,
    SessionExecutionLeaseAuthority, TurnAddress, TurnCancelClosureAuthorization,
    TurnCancelClosureProposal, TurnCancelIntentSnapshot, TurnId,
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
        &SessionExecutionLeaseAuthority {
            session_id: "recovery-session".into(),
            owner: LeaseOwnerIdentity::opaque("owner", "old-process"),
            executor_id: "old-executor".to_string(),
            lease_token: "old-lease".to_string(),
            fencing_token,
        },
    )
    .expect("valid closure authorization")
}

#[test]
fn recovery_retains_only_the_resumable_logical_turn_closures() {
    let root = TurnId::from("root-turn");
    assert!(is_resumable_turn_or_follow_on(&root, &root));
    assert!(is_resumable_turn_or_follow_on(
        &TurnId::from("root-turn:agent-frame:1"),
        &root,
    ));
    assert!(!is_resumable_turn_or_follow_on(
        &TurnId::from("root-turn-other"),
        &root,
    ));
    assert!(!is_resumable_turn_or_follow_on(
        &TurnId::from("unrelated-turn"),
        &root,
    ));
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
    let original_scope = ExecutionScope::process("original-process");
    let binding_id = crate::turn_control_binding_id_for_scope("binding", &original_scope)
        .expect("physical binding id");
    let preserved = closure_authorization("root-turn", &binding_id, original_scope, 7);
    let error = recovered_turn_cancel_closure(
        vec![preserved],
        &TurnAddress::new("recovery-session", "root-turn"),
        &binding_id,
        &ExecutionScope::process("successor-process"),
    )
    .expect_err("scope drift must fail closed");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::InvalidTurnCancelRequest
    );
}
