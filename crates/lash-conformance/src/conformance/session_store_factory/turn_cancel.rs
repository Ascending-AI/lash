use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::sync::Arc;

use super::session_store_request;
use pretty_assertions::assert_eq;

fn cancel_evidence(request: &crate::TurnCancelRequest) -> crate::TurnCancellationEvidence {
    crate::TurnCancellationEvidence {
        request_id: request.request_id.clone(),
        origin: request.origin.clone(),
        reason: request.reason.clone(),
        undelivered: request.undelivered,
        mode: request.mode,
        honoured_after_step: None,
    }
}

fn request_from_evidence_for_test(
    address: &crate::TurnAddress,
    evidence: &crate::TurnCancellationEvidence,
) -> crate::TurnCancelRequest {
    crate::TurnCancelRequest {
        address: address.clone(),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    }
}

const TURN_CANCEL_BINDING_ID: &str = "lash-conformance-turn-cancel-v1";

fn closure_key(
    address: &crate::TurnAddress,
    wait: crate::AwaitEventWaitIdentity,
    suffix: &str,
) -> crate::AwaitEventKey {
    crate::AwaitEventKey {
        scope: address.execution_scope(),
        wait,
        key_id: format!("{}:{suffix}", address.turn_id),
        signature: format!("conformance:{suffix}"),
    }
}

async fn authorize_closure(
    store: &Arc<dyn crate::RuntimePersistence>,
    fence: &crate::SessionExecutionLeaseAuthority,
    address: &crate::TurnAddress,
    observed: crate::TurnCancelIntentSnapshot,
    proposed: crate::TurnCancelClosureProposal,
) -> crate::TurnCancelClosureAuthorization {
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            fence,
            TURN_CANCEL_BINDING_ID,
            &address.execution_scope(),
        )
        .await
        .expect("bind cancellation authority under the current lease");
    let authorization = closure_authorization(
        address,
        address.execution_scope(),
        observed,
        proposed,
        fence,
    );
    store
        .authorize_turn_cancel_closure(fence, &authorization)
        .await
        .expect("persist exact closure authorization");
    authorization
}

fn closure_authorization(
    address: &crate::TurnAddress,
    admitted_scope: crate::ExecutionScope,
    observed: crate::TurnCancelIntentSnapshot,
    proposed: crate::TurnCancelClosureProposal,
    fence: &crate::SessionExecutionLeaseAuthority,
) -> crate::TurnCancelClosureAuthorization {
    let binding_id =
        crate::turn_control_binding_id_for_scope(TURN_CANCEL_BINDING_ID, &admitted_scope)
            .expect("bind physical cancellation scope");
    crate::TurnCancelClosureAuthorization::new(
        address.clone(),
        binding_id,
        admitted_scope,
        closure_key(
            address,
            crate::AwaitEventWaitIdentity::TurnCancelGate,
            "cancel",
        ),
        closure_key(
            address,
            crate::AwaitEventWaitIdentity::TurnCancelEscalation,
            "escalation",
        ),
        closure_key(
            address,
            crate::AwaitEventWaitIdentity::TurnTerminal,
            "terminal",
        ),
        proposed,
        observed,
        fence,
    )
    .expect("construct exact closure authorization")
}

fn settled_closure(
    authorization: &crate::TurnCancelClosureAuthorization,
    effective: Option<crate::TurnCancellationEvidence>,
) -> crate::TurnCancelClosureSettlement {
    let base = match authorization.proposed_base() {
        crate::TurnCancelClosureProposal::CancelRequested(evidence) => Some(evidence.clone()),
        crate::TurnCancelClosureProposal::CompletionSealed => None,
    };
    crate::TurnCancelClosureSettlement::settled_for_test(authorization.clone(), base, effective)
}

/// Replaying an already committed receipt must not consume a newer pending
/// authorization that happens to reuse the same turn address.
pub(super) async fn turn_cancel_exact_replay_preserves_different_pending_authorization(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-exact-replay-preserves-new-authorization"),
        "turn-cancel-exact-replay-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-exact-replay:turn"),
    );
    let first = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("exact-replay-first", "exact-replay-first:1"),
            "exact-replay-first:executor",
            60_000,
        )
        .await
        .expect("claim first exact-replay lane")
        .acquired()
        .expect("first exact-replay lane is free");
    let first_authorization = authorize_closure(
        &store,
        &first.fence(),
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &address.turn_id,
            "final",
        ))
        .expect("stamp exact replay operation");
    commit.interrupted_turn_input_turn_id = Some(address.turn_id.clone());
    commit.interrupted_turn_cancel_intent = Some(crate::TurnCancelIntentSnapshot::Absent);
    commit.turn_cancel_closure_settlement = Some(settled_closure(&first_authorization, None));
    commit.release_session_execution_lease = Some(first.completion());
    store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit first authorization");

    let successor = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque(
                "exact-replay-successor",
                "exact-replay-successor:1",
            ),
            "exact-replay-successor:executor",
            60_000,
        )
        .await
        .expect("claim successor exact-replay lane")
        .acquired()
        .expect("successor exact-replay lane is free");
    let successor_authorization = authorize_closure(
        &store,
        &successor.fence(),
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    assert_ne!(successor_authorization, first_authorization);
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read successor authorization before replay"),
        vec![successor_authorization.clone()]
    );

    store
        .commit_runtime_state(commit)
        .await
        .expect("adopt exact committed receipt");
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read successor authorization after replay"),
        vec![successor_authorization],
        "exact receipt replay must retain a different pending closure authorization"
    );
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("read head after exact replay")
            .expect("committed head exists")
            .head_revision,
        1,
        "exact replay must not publish another head"
    );
}

/// The durable closure slot is non-overwritable, survives lease-generation
/// changes, preserves its admitted physical scope, and can be consumed only by
/// a current owner presenting the exact authorization.
pub(super) async fn turn_cancel_closure_settlement_is_fenced_and_non_overwritable(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-closure-authorization"),
        "turn-cancel-closure-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let first = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("closure-first", "closure-first:incarnation"),
            "closure-first:executor",
            60_000,
        )
        .await
        .expect("claim first closure lane")
        .acquired()
        .expect("first closure lane is free");
    let physical_scope = crate::ExecutionScope::process("turn-cancel-shared-process");
    let selected_binding =
        crate::turn_control_binding_id_for_scope(TURN_CANCEL_BINDING_ID, &physical_scope)
            .expect("bind process scope");
    store
        .validate_turn_cancellation_binding(
            &request.session_id,
            &first.fence(),
            &selected_binding,
            &physical_scope,
        )
        .await
        .expect("bind the first authority");
    let turn = TurnId::from("turn-cancel-closure-authorization:first");
    let address = crate::TurnAddress::new(&request.session_id, &turn);
    let exact = closure_authorization(
        &address,
        physical_scope.clone(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &first.fence(),
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first.fence(), &exact)
            .await
            .expect("authorize vacant slot"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first.fence(), &exact)
            .await
            .expect("adopt exact retry"),
        crate::TurnCancelClosureAuthorizationOutcome::AdoptedExact
    );
    let conflicting = closure_authorization(
        &address,
        physical_scope.clone(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CancelRequested(crate::TurnCancellationEvidence {
            request_id: "conflicting-terminal".to_string(),
            origin: None,
            reason: None,
            undelivered: crate::TurnCancelDisposition::Defer,
            mode: crate::TurnCancelMode::Immediate,
            honoured_after_step: None,
        }),
        &first.fence(),
    );
    assert!(matches!(
        store
            .authorize_turn_cancel_closure(&first.fence(), &conflicting)
            .await,
        Err(crate::StoreError::TurnCancelClosureConflict { .. })
    ));
    assert_eq!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &first.fence(),
                &selected_binding,
                &physical_scope,
            )
            .await
            .expect("read exact pending authorization"),
        vec![exact.clone()]
    );
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("lifecycle sees the durable closure pin"),
        vec![exact.clone()]
    );
    assert_eq!(
        factory
            .pending_turn_cancel_closure_pins(&request.session_id)
            .await
            .expect("factory lifecycle inspection sees the durable closure pin"),
        vec![exact.clone()]
    );
    let delete_failure = factory
        .delete_session(&request.session_id)
        .await
        .expect_err("session deletion must refuse a live closure pin");
    assert!(matches!(
        delete_failure.stop,
        crate::MaintenanceStop::Failed(crate::StoreError::TurnCancelClosureLifecyclePinned {
            ref session_id,
            pending_count: 1,
        }) if session_id == request.session_id
    ));
    assert!(matches!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &first.fence(),
                &crate::turn_control_binding_id_for_scope(
                    TURN_CANCEL_BINDING_ID,
                    &crate::ExecutionScope::process("turn-cancel-wrong-successor-process"),
                )
                .expect("bind wrong successor physical scope"),
                &crate::ExecutionScope::process("turn-cancel-wrong-successor-process"),
            )
            .await,
        Err(crate::StoreError::TurnCancelBindingMismatch { .. })
    ));
    assert!(matches!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &first.fence(),
                &selected_binding,
                &crate::ExecutionScope::process("turn-cancel-wrong-successor-process"),
            )
            .await,
        Err(crate::StoreError::TurnCancelBindingMismatch { .. })
    ));

    store
        .release_session_execution_lease(&first.completion())
        .await
        .expect("release first owner");
    let successor = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque(
                "closure-successor",
                "closure-successor:incarnation",
            ),
            "closure-successor:executor",
            60_000,
        )
        .await
        .expect("claim successor closure lane")
        .acquired()
        .expect("successor closure lane is free");
    assert_eq!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &successor.fence(),
                &selected_binding,
                &physical_scope,
            )
            .await
            .expect("successor adopts pending authorization"),
        vec![exact.clone()]
    );
    assert!(matches!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &successor.fence(),
                &turn,
                &crate::TurnCancelIntentSnapshot::Absent,
                Some(&settled_closure(&conflicting, None)),
            )
            .await,
        Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch { .. })
    ));
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("wrong terminal cannot consume the authorization"),
        vec![exact.clone()]
    );
    store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &successor.fence(),
            &turn,
            &crate::TurnCancelIntentSnapshot::Absent,
            Some(&settled_closure(&exact, None)),
        )
        .await
        .expect("current successor consumes exact authorization")
        .into_applied()
        .expect("absent intent remains stable");
    assert!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &successor.fence(),
                &selected_binding,
                &physical_scope,
            )
            .await
            .expect("read drained closure slot")
            .is_empty()
    );
    assert!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("lifecycle pin retires only with successful repair")
            .is_empty()
    );
    assert!(
        factory
            .pending_turn_cancel_closure_pins(&request.session_id)
            .await
            .expect("factory lifecycle inspection sees the retired pin")
            .is_empty()
    );

    // Consuming the exact authorization is the durable fence. Lease takeover
    // alone cannot veto final settlement under ADR 0029.
    let stale_state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    let (mut stale_commit, _) = crate::RuntimeCommit::persisted_state_for_test(&stale_state, &[])
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &turn,
            "stale-closure-final",
        ))
        .expect("stamp stale closure commit operation");
    stale_commit.interrupted_turn_input_turn_id = Some(turn.clone());
    stale_commit.interrupted_turn_cancel_intent = Some(crate::TurnCancelIntentSnapshot::Absent);
    stale_commit.turn_cancel_closure_settlement = Some(settled_closure(&exact, None));
    stale_commit.release_session_execution_lease = Some(first.completion());
    assert!(matches!(
        store.commit_runtime_state(stale_commit).await,
        Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch { .. })
    ));
    let second_address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-closure-authorization:second"),
    );
    let stale = closure_authorization(
        &second_address,
        physical_scope.clone(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &first.fence(),
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first.fence(), &stale)
            .await
            .expect("advisory takeover does not fence closure authorization"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    let current = closure_authorization(
        &second_address,
        physical_scope,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &successor.fence(),
    );
    assert!(matches!(
        store
            .authorize_turn_cancel_closure(&successor.fence(), &current)
            .await,
        Err(crate::StoreError::TurnCancelClosureConflict { .. })
    ));
    store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &successor.fence(),
            &second_address.turn_id,
            &crate::TurnCancelIntentSnapshot::Absent,
            Some(&settled_closure(&stale, None)),
        )
        .await
        .expect("consume successor authorization")
        .into_applied()
        .expect("successor intent remains absent");

    // A session authority is stable across turns, while each authorization
    // retains the exact turn address recovered from durable input state.
    let session_request = session_store_request(
        &SessionId::from("turn-cancel-session-scope-variation"),
        "turn-cancel-session-scope-model",
        crate::SessionRelation::Root,
    );
    let session_store = factory
        .create_store(&session_request)
        .await
        .expect("create session-scoped store");
    let session_lease = session_store
        .try_claim_session_execution_lease(
            &session_request.session_id,
            &crate::LeaseOwnerIdentity::opaque(
                "session-scope-owner",
                "session-scope-owner:incarnation",
            ),
            "session-scope-executor",
            60_000,
        )
        .await
        .expect("claim session-scoped lane")
        .acquired()
        .expect("session-scoped lane is free");
    let first_session_address = crate::TurnAddress::new(
        &session_request.session_id,
        TurnId::from("session-scope:first"),
    );
    let second_session_address = crate::TurnAddress::new(
        &session_request.session_id,
        TurnId::from("session-scope:second"),
    );
    assert!(
        crate::TurnCancelClosureAuthorization::new(
            first_session_address.clone(),
            TURN_CANCEL_BINDING_ID,
            second_session_address.execution_scope(),
            closure_key(
                &first_session_address,
                crate::AwaitEventWaitIdentity::TurnCancelGate,
                "wrong-scope-cancel",
            ),
            closure_key(
                &first_session_address,
                crate::AwaitEventWaitIdentity::TurnCancelEscalation,
                "wrong-scope-escalation",
            ),
            closure_key(
                &first_session_address,
                crate::AwaitEventWaitIdentity::TurnTerminal,
                "wrong-scope-terminal",
            ),
            crate::TurnCancelClosureProposal::CompletionSealed,
            crate::TurnCancelIntentSnapshot::Absent,
            &session_lease.fence(),
        )
        .is_err()
    );
    for address in [&first_session_address, &second_session_address] {
        session_store
            .validate_turn_cancellation_binding(
                &session_request.session_id,
                &session_lease.fence(),
                TURN_CANCEL_BINDING_ID,
                &address.execution_scope(),
            )
            .await
            .expect("same session authority admits a distinct turn");
        let authorization = closure_authorization(
            address,
            address.execution_scope(),
            crate::TurnCancelIntentSnapshot::Absent,
            crate::TurnCancelClosureProposal::CompletionSealed,
            &session_lease.fence(),
        );
        assert_eq!(authorization.admitted_scope(), &address.execution_scope());
        session_store
            .authorize_turn_cancel_closure(&session_lease.fence(), &authorization)
            .await
            .expect("authorize exact session turn");
        session_store
            .repair_orphaned_active_turn_inputs(
                &session_request.session_id,
                &session_lease.fence(),
                &address.turn_id,
                &crate::TurnCancelIntentSnapshot::Absent,
                Some(&settled_closure(&authorization, None)),
            )
            .await
            .expect("consume exact session-turn authorization")
            .into_applied()
            .expect("session-turn intent remains absent");
    }
}

/// An escalation can change effective timing while repair retains the first
/// accepted request and its provenance across owner loss and reopen.
pub(super) async fn turn_cancel_repair_preserves_base_across_escalation_and_reopen(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    async fn claim(
        store: &Arc<dyn crate::RuntimePersistence>,
        session_id: &SessionId,
        owner: &str,
    ) -> crate::SessionExecutionLease {
        store
            .try_claim_session_execution_lease(
                session_id,
                &crate::LeaseOwnerIdentity::opaque(owner, format!("{owner}:incarnation")),
                "repair-base-executor",
                60_000,
            )
            .await
            .expect("claim repair base lane")
            .acquired()
            .expect("repair base lane is free")
    }
    let request = session_store_request(
        &SessionId::from("turn-cancel-repair-base-acceptor"),
        "turn-cancel-repair-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let turn_id = TurnId::from("turn-cancel-repair-base-acceptor:turn");
    let address = crate::TurnAddress::new(&request.session_id, &turn_id);
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("repair after escalation"),
        ))
        .await
        .expect("enqueue interrupted input");
    let base = crate::TurnCancelRequest::new(address.clone(), "repair-base-after-step", None)
        .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(base.clone())
        .await
        .expect("persist base acceptor");
    let escalation =
        crate::TurnCancelRequest::new(address.clone(), "repair-effective-immediate", None);
    store
        .record_turn_cancel_request(escalation.clone())
        .await
        .expect("advance escalation revision");
    let observed = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot escalated intent");
    assert!(matches!(
        &observed,
        crate::TurnCancelIntentSnapshot::Present { request, revision: 2 }
            if request == &base
    ));
    let authorizing = claim(&store, &request.session_id, "repair-base-owner").await;
    let authorization = authorize_closure(
        &store,
        &authorizing.fence(),
        &address,
        observed.clone(),
        crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&base)),
    )
    .await;
    store
        .release_session_execution_lease(&authorizing.completion())
        .await
        .expect("release crashed owner");
    drop(store);

    let reopened = factory
        .open_existing_store(&request)
        .await
        .expect("reopen repair store")
        .expect("repair store exists");
    let successor = claim(&reopened, &request.session_id, "repair-base-successor").await;
    reopened
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &successor.fence(),
            &turn_id,
            &observed,
            Some(&crate::TurnCancelClosureSettlement::settled_for_test(
                authorization,
                Some(cancel_evidence(&base)),
                Some(cancel_evidence(&escalation)),
            )),
        )
        .await
        .expect("repair with authenticated effective escalation")
        .into_applied()
        .expect("escalated intent remains stable");
    assert_eq!(
        reopened
            .turn_cancel_request(&address)
            .await
            .expect("read repaired base")
            .expect("repaired base remains present")
            .request,
        base,
    );
}

/// Process-scope retirement and cancellation authorization are one
/// linearizable lifecycle boundary. Both deterministic orders and a
/// barrier-released overlap have exactly one winner, and retirement remains a
/// permanent admission refusal for that physical scope.
pub(super) async fn turn_cancel_scope_retirement_serializes_with_authorization(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    async fn prepared(
        factory: &Arc<dyn crate::SessionStoreFactory>,
        suffix: &str,
        scope: crate::ExecutionScope,
    ) -> (
        Arc<dyn crate::RuntimePersistence>,
        crate::SessionExecutionLease,
        crate::TurnCancelClosureAuthorization,
    ) {
        let request = session_store_request(
            &SessionId::from(format!("turn-cancel-scope-retirement:{suffix}")),
            "turn-cancel-scope-retirement-model",
            crate::SessionRelation::Root,
        );
        let store = factory.create_store(&request).await.expect("create store");
        let lease = store
            .try_claim_session_execution_lease(
                &request.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    format!("scope-retirement:{suffix}"),
                    format!("scope-retirement:{suffix}:incarnation"),
                ),
                "scope-retirement:executor",
                60_000,
            )
            .await
            .expect("claim lifecycle lane")
            .acquired()
            .expect("lifecycle lane is free");
        let binding = crate::turn_control_binding_id_for_scope(TURN_CANCEL_BINDING_ID, &scope)
            .expect("bind physical scope");
        store
            .validate_turn_cancellation_binding(
                &request.session_id,
                &lease.fence(),
                &binding,
                &scope,
            )
            .await
            .expect("select physical cancellation owner");
        let address = crate::TurnAddress::new(
            &request.session_id,
            TurnId::from(format!("turn-cancel-scope-retirement:{suffix}:turn")),
        );
        let authorization = closure_authorization(
            &address,
            scope,
            crate::TurnCancelIntentSnapshot::Absent,
            crate::TurnCancelClosureProposal::CompletionSealed,
            &lease.fence(),
        );
        (store, lease, authorization)
    }

    let authorization_first_scope =
        crate::ExecutionScope::process("turn-cancel-scope-retirement:authorization-first");
    let (authorization_first_store, authorization_first_lease, authorization_first) = prepared(
        &factory,
        "authorization-first",
        authorization_first_scope.clone(),
    )
    .await;
    authorization_first_store
        .authorize_turn_cancel_closure(&authorization_first_lease.fence(), &authorization_first)
        .await
        .expect("authorization wins before retirement");
    assert!(matches!(
        factory
            .retire_turn_cancel_closure_scope(&authorization_first_scope)
            .await,
        Err(crate::StoreError::TurnCancelClosureLifecyclePinned { .. })
    ));
    authorization_first_store
        .repair_orphaned_active_turn_inputs(
            authorization_first.session_id(),
            &authorization_first_lease.fence(),
            authorization_first.turn_id(),
            &crate::TurnCancelIntentSnapshot::Absent,
            Some(&settled_closure(&authorization_first, None)),
        )
        .await
        .expect("consume authorized closure")
        .into_applied()
        .expect("absent intent remains stable");
    factory
        .retire_turn_cancel_closure_scope(&authorization_first_scope)
        .await
        .expect("retire after the pin is consumed");

    let retired_first_scope =
        crate::ExecutionScope::process("turn-cancel-scope-retirement:retired-first");
    factory
        .retire_turn_cancel_closure_scope(&retired_first_scope)
        .await
        .expect("retirement wins before authorization");
    let (retired_first_store, retired_first_lease, retired_first) =
        prepared(&factory, "retired-first", retired_first_scope).await;
    assert!(matches!(
        retired_first_store
            .authorize_turn_cancel_closure(&retired_first_lease.fence(), &retired_first)
            .await,
        Err(crate::StoreError::TurnCancelClosureScopeRetired { .. })
    ));
    assert!(
        retired_first_store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("inspect retirement-first pins")
            .is_empty()
    );

    let overlapping_scope =
        crate::ExecutionScope::process("turn-cancel-scope-retirement:overlapping");
    let (overlapping_store, overlapping_lease, overlapping) =
        prepared(&factory, "overlapping", overlapping_scope.clone()).await;
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let authorize_start = Arc::clone(&start);
    let retire_start = Arc::clone(&start);
    let retire_factory = Arc::clone(&factory);
    let authorize_store = Arc::clone(&overlapping_store);
    let authorize_fence = overlapping_lease.fence();
    let authorize_value = overlapping.clone();
    let retire_scope = overlapping_scope.clone();
    let (authorized, retired) = tokio::join!(
        async move {
            authorize_start.wait().await;
            authorize_store
                .authorize_turn_cancel_closure(&authorize_fence, &authorize_value)
                .await
        },
        async move {
            retire_start.wait().await;
            retire_factory
                .retire_turn_cancel_closure_scope(&retire_scope)
                .await
        }
    );
    match (authorized, retired) {
        (Ok(_), Err(crate::StoreError::TurnCancelClosureLifecyclePinned { .. })) => {
            assert_eq!(
                overlapping_store
                    .pending_turn_cancel_closure_pins()
                    .await
                    .expect("read overlapping winner"),
                vec![overlapping]
            );
        }
        (Err(crate::StoreError::TurnCancelClosureScopeRetired { .. }), Ok(())) => {
            assert!(
                overlapping_store
                    .pending_turn_cancel_closure_pins()
                    .await
                    .expect("read overlapping retirement winner")
                    .is_empty()
            );
        }
        outcomes => panic!("scope lifecycle race did not linearize: {outcomes:?}"),
    }
}

/// Every persisted disposition survives the owner crash that separates cancel
/// observation from repair. The reopened repair applies the requested policy
/// only to the undelivered active-turn row, records its payload in the durable
/// cancel outcome, and leaves already-next-turn work untouched.
pub(super) async fn turn_cancel_disposition_crash_matrix(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    #[derive(Clone, Copy, Debug)]
    enum RepairPath {
        Commit,
        Teardown,
        CrashBeforeRepair,
    }
    for mode in [
        crate::TurnCancelMode::Immediate,
        crate::TurnCancelMode::AfterStep,
    ] {
        for disposition in [
            crate::TurnCancelDisposition::Defer,
            crate::TurnCancelDisposition::Drop,
        ] {
            for path in [
                RepairPath::Commit,
                RepairPath::Teardown,
                RepairPath::CrashBeforeRepair,
            ] {
                turn_cancel_disposition_crash_cell(Arc::clone(&factory), mode, disposition, path)
                    .await;
            }
        }
    }

    async fn turn_cancel_disposition_crash_cell(
        factory: Arc<dyn crate::SessionStoreFactory>,
        mode: crate::TurnCancelMode,
        disposition: crate::TurnCancelDisposition,
        path: RepairPath,
    ) {
        let suffix = format!("{:?}-{:?}-{:?}", mode, disposition, path).to_ascii_lowercase();
        let request = session_store_request(
            &SessionId::from(format!("turn-cancel-{suffix}")),
            "turn-cancel-drop-model",
            crate::SessionRelation::Root,
        );
        let turn_id = TurnId::from(format!("turn-cancel-{suffix}:turn"));
        let store = factory
            .create_store(&request)
            .await
            .expect("create cancellation crash store");
        let dropped = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::active_turn(
                    &turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("restore this unsent steer"),
            ))
            .await
            .expect("enqueue active-turn input");
        let untouched = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &request.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("already queued for next turn"),
            ))
            .await
            .expect("enqueue next-turn input");
        let cancel = crate::TurnCancelRequest::new(
            crate::TurnAddress::new(&request.session_id, &turn_id),
            format!("turn-cancel-{suffix}:request"),
            Some("conformance-host".to_string()),
        )
        .undelivered(disposition)
        .mode(mode);
        store
            .record_turn_cancel_request(cancel.clone())
            .await
            .expect("persist the disposition before the owner crashes");
        let authorizing_lease = store
            .try_claim_session_execution_lease(
                &request.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "turn-cancel-authorizer",
                    format!("turn-cancel-authorizer:{suffix}"),
                ),
                "turn-cancel-authorizer-executor",
                60_000,
            )
            .await
            .expect("claim authorization lane")
            .acquired()
            .expect("authorization lane is free");
        let observed_authorization = store
            .turn_cancel_request_intent(&cancel.address)
            .await
            .expect("snapshot cancellation intent before authorization");
        let closure_authorization = authorize_closure(
            &store,
            &authorizing_lease.fence(),
            &cancel.address,
            observed_authorization,
            crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&cancel)),
        )
        .await;
        if matches!(path, RepairPath::Commit) {
            let mut state = crate::RuntimeSessionState {
                session_id: request.session_id.clone(),
                ..crate::RuntimeSessionState::new(request.policy.clone())
            };
            state.ensure_agent_frame_initialized();
            let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .with_operation(crate::OperationId::turn(
                    &request.session_id,
                    &turn_id,
                    "final",
                ))
                .expect("stamp exact turn final operation");
            commit.interrupted_turn_input_turn_id = Some(turn_id.clone());
            commit.interrupted_turn_input_cancellation = Some(cancel_evidence(&cancel));
            commit.interrupted_turn_cancel_intent = Some(
                store
                    .turn_cancel_request_intent(&cancel.address)
                    .await
                    .expect("snapshot cancellation intent before final commit"),
            );
            commit.release_session_execution_lease = Some(authorizing_lease.completion());
            commit.turn_cancel_closure_settlement = Some(settled_closure(
                &closure_authorization,
                Some(cancel_evidence(&cancel)),
            ));
            let receipt = store
                .commit_runtime_state(commit)
                .await
                .expect("cancel final commit");
            assert_eq!(receipt.turn_cancel_input_outcome.len(), 1);
        } else {
            store
                .release_session_execution_lease(&authorizing_lease.completion())
                .await
                .expect("release authorizing owner before successor repair");
        }
        if matches!(path, RepairPath::CrashBeforeRepair) {
            drop(store);
        }
        let reopened = factory
            .open_existing_store(&request)
            .await
            .expect("reopen cancellation store")
            .expect("cancel request admitted the session");
        if matches!(path, RepairPath::CrashBeforeRepair) {
            reopened
                .vacuum()
                .await
                .expect("vacuum preserves unresolved cancellation intent");
            assert!(
                reopened
                    .turn_cancel_request(&cancel.address)
                    .await
                    .expect("read request after vacuum")
                    .is_some(),
                "unresolved cancellation intent must survive vacuum"
            );
        }
        let lease = reopened
            .try_claim_session_execution_lease(
                &request.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "turn-cancel-drop-successor",
                    "turn-cancel-drop-successor:incarnation",
                ),
                "turn-cancel-drop-successor-executor",
                60_000,
            )
            .await
            .expect("claim successor lane")
            .acquired()
            .expect("successor lane is free");
        let observed = reopened
            .turn_cancel_request_intent(&cancel.address)
            .await
            .expect("snapshot cancellation intent before repair");
        let outcome = if matches!(path, RepairPath::Commit) {
            reopened
                .turn_cancel_request(&cancel.address)
                .await
                .expect("read committed cancel")
                .expect("durable cancel")
                .outcome
                .expect("commit outcome")
        } else {
            reopened
                .repair_orphaned_active_turn_inputs(
                    &request.session_id,
                    &lease.fence(),
                    &turn_id,
                    &observed,
                    Some(&settled_closure(
                        &closure_authorization,
                        Some(cancel_evidence(&cancel)),
                    )),
                )
                .await
                .expect("repair the dead turn")
                .into_applied()
                .expect("intent remains unchanged")
        };
        assert_eq!(outcome.affected_inputs.len(), 1);
        let affected = &outcome.affected_inputs[0];
        assert_eq!(affected.input_id, dropped.input_id);
        assert_eq!(affected.disposition, disposition);
        assert_eq!(
            serde_json::to_value(&affected.payload).expect("encode affected payload"),
            serde_json::to_value(&dropped.input).expect("encode submitted payload"),
            "the teardown repair must return the exact dropped payload"
        );
        let durable = reopened
            .turn_cancel_request(&cancel.address)
            .await
            .expect("read durable cancel request")
            .expect("cancel request survives reopen");
        assert_eq!(
            durable.request, cancel,
            "{suffix}: the durable row carries the requested mode"
        );
        assert_eq!(
            serde_json::to_value(&durable.outcome).expect("encode durable cancel outcome"),
            serde_json::to_value(Some(&outcome)).expect("encode repair outcome"),
        );
        assert_eq!(
            reopened
                .list_pending_turn_inputs(&request.session_id)
                .await
                .expect("list pending inputs after repair")
                .into_iter()
                .map(|input| input.input_id)
                .collect::<Vec<_>>(),
            match disposition {
                crate::TurnCancelDisposition::Defer => vec![dropped.input_id, untouched.input_id],
                crate::TurnCancelDisposition::Drop => vec![untouched.input_id],
            },
            "cancel repair applies disposition only to ActiveTurn and never touches NextTurn"
        );
        if matches!(path, RepairPath::Commit) {
            reopened
                .vacuum()
                .await
                .expect("vacuum the committed input payload tombstone");
            let late = crate::TurnCancelRequest::new(
                cancel.address.clone(),
                format!("turn-cancel-{suffix}:late"),
                None,
            );
            let no_op = reopened
                .record_turn_cancel_request(late.clone())
                .await
                .expect("committed request does not decode reclaimed outcome payloads");
            assert_eq!(no_op.request, late);
            assert!(no_op.outcome.is_none());

            // A host can pre-name the same turn again after its prior affected
            // payload tombstone was reclaimed. Recovery needs only intent to
            // consult the existing gate; reconstructing the historical
            // outcome here would fail on PostgreSQL by design.
            let later = reopened
                .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                    &request.session_id,
                    crate::TurnInputIngress::active_turn(
                        &turn_id,
                        crate::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    crate::TurnInput::text("same turn id after prior repair vacuum"),
                ))
                .await
                .expect("enqueue later active-turn input");
            let intent = reopened
                .turn_cancel_request_intent(&cancel.address)
                .await
                .expect("read intent without historical payload reconstruction");
            assert_eq!(intent.request(), Some(&cancel));
            let later_authorization = authorize_closure(
                &reopened,
                &lease.fence(),
                &cancel.address,
                intent.clone(),
                crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&cancel)),
            )
            .await;
            let repaired = reopened
                .repair_orphaned_active_turn_inputs(
                    &request.session_id,
                    &lease.fence(),
                    &turn_id,
                    &intent,
                    Some(&settled_closure(
                        &later_authorization,
                        Some(cancel_evidence(&cancel)),
                    )),
                )
                .await
                .expect("repair later input from retained winner intent")
                .into_applied()
                .expect("retained intent remains unchanged");
            assert_eq!(repaired.affected_inputs[0].input_id, later.input_id);
            assert_eq!(repaired.affected_inputs[0].disposition, disposition);
        }
    }
}

/// Escalating a durable after-step request advances the intent revision while
/// the one durable row retains the first request and its provenance.
pub(super) async fn turn_cancel_request_escalation_advances_intent_without_replacing_base(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-escalation"),
        "turn-cancel-escalation-model",
        crate::SessionRelation::Root,
    );
    let turn_id = "turn-cancel-escalation:turn";
    let store = factory
        .create_store(&request)
        .await
        .expect("create escalation store");
    let weaker_winner_address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-unaccepted-stronger"),
    );
    let unaccepted_immediate = crate::TurnCancelRequest::new(
        weaker_winner_address.clone(),
        "turn-cancel-unaccepted-stronger:A",
        None,
    )
    .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(unaccepted_immediate)
        .await
        .expect("persist unaccepted immediate intent");
    let observed_unaccepted = store
        .turn_cancel_request_intent(&weaker_winner_address)
        .await
        .expect("snapshot unaccepted immediate intent");
    let accepted_after_step = crate::TurnCancellationEvidence {
        request_id: "turn-cancel-unaccepted-stronger:B".to_string(),
        origin: None,
        reason: None,
        undelivered: crate::TurnCancelDisposition::Defer,
        mode: crate::TurnCancelMode::AfterStep,
        honoured_after_step: None,
    };
    assert!(
        store
            .reconcile_turn_cancel_winner(
                &weaker_winner_address,
                &observed_unaccepted,
                &accepted_after_step,
            )
            .await
            .expect("project accepted after-step winner over unaccepted immediate intent")
    );
    assert_eq!(
        store
            .turn_cancel_request(&weaker_winner_address)
            .await
            .expect("read projected weaker winner")
            .expect("projected winner exists")
            .request
            .request_id,
        accepted_after_step.request_id,
    );
    let address = crate::TurnAddress::new(&request.session_id, turn_id);
    let stop = crate::TurnCancelRequest::new(
        address.clone(),
        "turn-cancel-escalation:stop",
        Some("conformance-host".to_string()),
    )
    .with_reason("stop after the step")
    .undelivered(crate::TurnCancelDisposition::Drop)
    .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(stop.clone())
        .await
        .expect("persist the after-step request");
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request")
        .expect("after-step request persisted");
    assert_eq!(durable.request, stop);
    assert!(durable.outcome.is_none());

    let weaker_again = crate::TurnCancelRequest::new(
        address.clone(),
        "turn-cancel-escalation:stop-again",
        Some("conformance-host".to_string()),
    )
    .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(weaker_again)
        .await
        .expect("record a same-strength request");
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request")
        .expect("request persists");
    assert_eq!(
        durable.request, stop,
        "a same-strength request never replaces the first writer"
    );
    let stale_observed = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot the delayed base projection");

    let abort = crate::TurnCancelRequest::new(
        address.clone(),
        "turn-cancel-escalation:abort",
        Some("conformance-operator".to_string()),
    )
    .with_reason("escalated to abort")
    .undelivered(crate::TurnCancelDisposition::Defer);
    store
        .record_turn_cancel_request(abort.clone())
        .await
        .expect("record the effective escalation");
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request")
        .expect("request persists");
    assert_eq!(
        durable.request, stop,
        "an immediate escalation cannot replace the original policy acceptor"
    );
    assert!(durable.outcome.is_none());

    let stale_base = crate::TurnCancellationEvidence {
        request_id: "turn-cancel-escalation:stale-base".to_string(),
        origin: Some("conformance-host".to_string()),
        reason: Some("delayed base-gate projection".to_string()),
        undelivered: abort.undelivered,
        mode: crate::TurnCancelMode::AfterStep,
        honoured_after_step: None,
    };
    assert!(
        !store
            .reconcile_turn_cancel_winner(&address, &stale_observed, &stale_base)
            .await
            .expect("reject a delayed base-gate projection")
    );
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request after stale projection")
        .expect("request persists");
    assert_eq!(
        durable.request, stop,
        "a delayed projection cannot replace the immutable base acceptor"
    );

    // A gate-authoritative base projection may replace an unaccepted ingress
    // header. A later timing escalation advances freshness without replacing
    // that projected acceptor.
    let original_a = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot A before the ABA schedule");
    assert!(
        store
            .reconcile_turn_cancel_winner(&address, &original_a, &stale_base)
            .await
            .expect("reconcile current base with current authority")
    );
    store
        .record_turn_cancel_request(abort.clone())
        .await
        .expect("record another effective escalation");
    let current_a = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("read projected base after escalation");
    assert_eq!(
        current_a.request(),
        Some(&request_from_evidence_for_test(&address, &stale_base))
    );
    assert_ne!(
        current_a, original_a,
        "projection and escalation advance freshness"
    );
    assert!(
        !store
            .reconcile_turn_cancel_winner(&address, &original_a, &stale_base)
            .await
            .expect("reject stale A after ABA"),
        "a stale projection must not pass merely because request bytes returned to A"
    );

    let downgrade = crate::TurnCancelRequest::new(
        address.clone(),
        "turn-cancel-escalation:late-stop",
        Some("conformance-host".to_string()),
    )
    .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(downgrade)
        .await
        .expect("record a weaker request after the upgrade");
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request")
        .expect("request persists");
    assert_eq!(
        durable.request,
        request_from_evidence_for_test(&address, &stale_base),
        "a weaker request never replaces the base acceptor"
    );

    // Reopen models an owner crash after escalation: the projected base
    // acceptor is what the successor reads.
    drop(store);
    let reopened = factory
        .open_existing_store(&request)
        .await
        .expect("reopen escalation store")
        .expect("session admitted");
    let durable = reopened
        .turn_cancel_request(&address)
        .await
        .expect("read durable request after reopen")
        .expect("request survives reopen");
    assert_eq!(
        durable.request,
        request_from_evidence_for_test(&address, &stale_base)
    );
}

/// Ordinary orphan repair and cancellation intent serialize in the store.
/// A request committed first blocks no-intent repair until the gate decision
/// arrives; a request committed after ordinary repair cannot retroactively
/// dispose an input that no longer targets the turn.
pub(super) async fn turn_cancel_repair_orders_intent_and_ordinary_redefer(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    async fn lease(
        store: &Arc<dyn crate::RuntimePersistence>,
        session_id: &SessionId,
        owner: &str,
    ) -> crate::SessionExecutionLeaseAuthority {
        store
            .try_claim_session_execution_lease(
                session_id,
                &crate::LeaseOwnerIdentity::opaque(owner, format!("{owner}:incarnation")),
                &format!("{owner}:executor"),
                60_000,
            )
            .await
            .expect("claim repair lane")
            .acquired()
            .expect("repair lane is free")
            .fence()
    }

    let request = session_store_request(
        &SessionId::from("turn-cancel-intent-first"),
        "turn-cancel-repair-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let turn_id = TurnId::from("turn-cancel-intent-first:turn");
    let row = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("intent wins before repair"),
        ))
        .await
        .expect("enqueue active-turn input");
    let cancel = crate::TurnCancelRequest::new(
        crate::TurnAddress::new(&request.session_id, &turn_id),
        "turn-cancel-intent-first:request",
        None,
    )
    .undelivered(crate::TurnCancelDisposition::Drop)
    .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(cancel.clone())
        .await
        .expect("persist request before repair");
    let fence = lease(&store, &request.session_id, "intent-first-owner").await;
    let stale_absent = crate::TurnCancelIntentSnapshot::Absent;
    assert_eq!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &fence,
                &turn_id,
                &stale_absent,
                None,
            )
            .await
            .expect("no-intent repair observes concurrent intent"),
        crate::TurnCancelRepairResult::IntentChanged,
        "durable intent must veto ordinary repair"
    );
    let pending = store
        .list_pending_turn_inputs(&request.session_id)
        .await
        .expect("read input after veto");
    assert_eq!(pending[0].state, crate::TurnInputState::PendingActive);
    let stale_after_step = store
        .turn_cancel_request_intent(&cancel.address)
        .await
        .expect("snapshot after-step intent before escalation");
    let closure_authorization = authorize_closure(
        &store,
        &fence,
        &cancel.address,
        stale_after_step.clone(),
        crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&cancel)),
    )
    .await;
    let stronger_cancel = crate::TurnCancelRequest::new(
        cancel.address.clone(),
        "turn-cancel-intent-first:immediate",
        Some("conformance-operator".to_string()),
    )
    .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(stronger_cancel.clone())
        .await
        .expect("escalate intent before stale repair");
    assert_eq!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &fence,
                &turn_id,
                &stale_after_step,
                Some(&settled_closure(
                    &closure_authorization,
                    Some(cancel_evidence(&cancel)),
                )),
            )
            .await
            .expect("stale after-step repair observes immediate escalation"),
        crate::TurnCancelRepairResult::IntentChanged
    );
    let still_pending = store
        .list_pending_turn_inputs(&request.session_id)
        .await
        .expect("stale repair publishes no input effects");
    assert_eq!(still_pending[0].state, crate::TurnInputState::PendingActive);
    let observed = store
        .turn_cancel_request_intent(&cancel.address)
        .await
        .expect("refresh immediate intent after stale repair refusal");
    let cancelled = store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &fence,
            &turn_id,
            &observed,
            Some(&settled_closure(
                &closure_authorization,
                Some(cancel_evidence(&stronger_cancel)),
            )),
        )
        .await
        .expect("apply authoritative gate winner")
        .into_applied()
        .expect("refreshed intent remains unchanged");
    assert_eq!(cancelled.affected_inputs[0].input_id, row.input_id);
    assert_eq!(
        cancelled.affected_inputs[0].disposition,
        crate::TurnCancelDisposition::Drop
    );

    let request = session_store_request(
        &SessionId::from("turn-cancel-repair-first"),
        "turn-cancel-repair-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let turn_id = TurnId::from("turn-cancel-repair-first:turn");
    let row = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("ordinary repair wins before intent"),
        ))
        .await
        .expect("enqueue active-turn input");
    let fence = lease(&store, &request.session_id, "repair-first-owner").await;
    let repaired = store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &fence,
            &turn_id,
            &crate::TurnCancelIntentSnapshot::Absent,
            None,
        )
        .await
        .expect("ordinary repair before request")
        .into_applied()
        .expect("absent intent remains absent");
    assert_eq!(repaired.affected_inputs[0].input_id, row.input_id);
    assert_eq!(
        repaired.affected_inputs[0].disposition,
        crate::TurnCancelDisposition::Defer
    );
    let cancel = crate::TurnCancelRequest::new(
        crate::TurnAddress::new(&request.session_id, &turn_id),
        "turn-cancel-repair-first:request",
        None,
    )
    .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(cancel.clone())
        .await
        .expect("persist request after repair");
    let late_observed = store
        .turn_cancel_request_intent(&cancel.address)
        .await
        .expect("snapshot late cancellation intent");
    let late_authorization = authorize_closure(
        &store,
        &fence,
        &cancel.address,
        late_observed.clone(),
        crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&cancel)),
    )
    .await;
    assert!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &fence,
                &turn_id,
                &late_observed,
                Some(&settled_closure(
                    &late_authorization,
                    Some(cancel_evidence(&cancel)),
                )),
            )
            .await
            .expect("late winner sees no targeted input")
            .into_applied()
            .expect("late intent remains unchanged")
            .is_empty()
    );
    let durable = store
        .turn_cancel_request(&cancel.address)
        .await
        .expect("read late request")
        .expect("late request persisted");
    assert!(durable.outcome.is_none());

    let request = session_store_request(
        &SessionId::from("turn-cancel-completion-wins"),
        "turn-cancel-repair-model",
        crate::SessionRelation::Root,
    );
    let store = factory.create_store(&request).await.expect("create store");
    let turn_id = TurnId::from("turn-cancel-completion-wins:turn");
    let row = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("completion defeated a stale drop intent"),
        ))
        .await
        .expect("enqueue active-turn input");
    let stale_drop = crate::TurnCancelRequest::new(
        crate::TurnAddress::new(&request.session_id, &turn_id),
        "turn-cancel-completion-wins:request",
        None,
    )
    .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(stale_drop.clone())
        .await
        .expect("persist losing drop intent");
    let fence = lease(&store, &request.session_id, "completion-owner").await;
    let completion_observed = store
        .turn_cancel_request_intent(&stale_drop.address)
        .await
        .expect("snapshot losing cancellation intent");
    let completion_authorization = authorize_closure(
        &store,
        &fence,
        &stale_drop.address,
        completion_observed.clone(),
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let repaired = store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &fence,
            &turn_id,
            &completion_observed,
            Some(&settled_closure(&completion_authorization, None)),
        )
        .await
        .expect("apply completion gate decision")
        .into_applied()
        .expect("losing intent remains unchanged");
    assert_eq!(repaired.affected_inputs[0].input_id, row.input_id);
    assert_eq!(
        repaired.affected_inputs[0].disposition,
        crate::TurnCancelDisposition::Defer,
        "a losing request row cannot select Drop"
    );
    assert!(
        store
            .turn_cancel_request(&stale_drop.address)
            .await
            .expect("read losing intent")
            .expect("losing intent retained")
            .outcome
            .is_none(),
        "a losing intent must not acquire a cancellation outcome"
    );
}

/// A stale cancellation predicate refuses the entire final commit, including
/// head publication and active-input settlement. Refreshing only the predicate
/// and gate evidence then commits the already-materialized payload once.
pub(super) async fn turn_cancel_final_commit_intent_cas_is_atomic(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-final-cas"),
        "turn-cancel-final-cas-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create CAS store");
    let turn_id = TurnId::from("turn-cancel-final-cas:turn");
    let address = crate::TurnAddress::new(&request.session_id, &turn_id);
    let pending = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::active_turn(
                &turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("must settle exactly once"),
        ))
        .await
        .expect("enqueue active-turn input");
    let after_step =
        crate::TurnCancelRequest::new(address.clone(), "turn-cancel-final-cas:after-step", None)
            .mode(crate::TurnCancelMode::AfterStep);
    store
        .record_turn_cancel_request(after_step.clone())
        .await
        .expect("persist after-step intent");
    let stale = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot after-step intent");
    let lease = store
        .try_claim_session_execution_lease(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque(
                "turn-cancel-final-cas-owner",
                "turn-cancel-final-cas-owner:incarnation",
            ),
            "turn-cancel-final-cas-executor",
            60_000,
        )
        .await
        .expect("claim final CAS lane")
        .acquired()
        .expect("final CAS lane is free");
    let closure_authorization = authorize_closure(
        &store,
        &lease.fence(),
        &address,
        stale.clone(),
        crate::TurnCancelClosureProposal::CancelRequested(cancel_evidence(&after_step)),
    )
    .await;

    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &turn_id,
            "final",
        ))
        .expect("stamp final operation");
    commit.interrupted_turn_input_turn_id = Some(turn_id.clone());
    commit.interrupted_turn_input_cancellation = Some(cancel_evidence(&after_step));
    commit.interrupted_turn_cancel_intent = Some(stale);
    commit.release_session_execution_lease = Some(lease.completion());
    commit.turn_cancel_closure_settlement = Some(settled_closure(
        &closure_authorization,
        Some(cancel_evidence(&after_step)),
    ));

    let immediate =
        crate::TurnCancelRequest::new(address.clone(), "turn-cancel-final-cas:immediate", None)
            .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(immediate.clone())
        .await
        .expect("change intent before final commit");
    let before_head = store
        .load_session_head_meta()
        .await
        .expect("read head before stale commit")
        .map(|head| (head.head_revision, head.leaf_node_id));
    assert!(matches!(
        store.commit_runtime_state(commit.clone()).await,
        Err(crate::StoreError::TurnCancelIntentChanged { .. })
    ));
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("read head after stale commit")
            .map(|head| (head.head_revision, head.leaf_node_id)),
        before_head,
        "a stale cancellation predicate publishes no head effect"
    );
    let rows = store
        .list_pending_turn_inputs(&request.session_id)
        .await
        .expect("read active input after stale commit");
    assert_eq!(rows[0].input_id, pending.input_id);
    assert_eq!(rows[0].state, crate::TurnInputState::PendingActive);
    assert!(
        !store
            .turn_is_committed(&address)
            .await
            .expect("read receipt")
    );

    commit.interrupted_turn_cancel_intent = Some(
        store
            .turn_cancel_request_intent(&address)
            .await
            .expect("refresh cancellation predicate"),
    );
    commit.interrupted_turn_input_cancellation = Some(cancel_evidence(&immediate));
    commit.turn_cancel_closure_settlement =
        Some(crate::TurnCancelClosureSettlement::settled_for_test(
            closure_authorization.clone(),
            Some(cancel_evidence(&after_step)),
            Some(cancel_evidence(&immediate)),
        ));
    let receipt = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit with refreshed cancellation authority");
    assert_eq!(receipt.turn_cancel_input_outcome.len(), 1);
    assert_eq!(
        receipt.turn_cancel_input_outcome.affected_inputs[0].input_id,
        pending.input_id
    );
    assert_eq!(
        receipt.turn_cancel_input_outcome.affected_inputs[0].disposition,
        crate::TurnCancelDisposition::Drop
    );
    assert_eq!(
        store
            .turn_cancel_request(&address)
            .await
            .expect("read committed base acceptor")
            .expect("base acceptor remains projected")
            .request,
        after_step,
        "final settlement keeps the immutable base request while applying the escalation"
    );
    drop(store);
    let reopened = factory
        .open_existing_store(&request)
        .await
        .expect("reopen final settlement store")
        .expect("final settlement store remains present");
    assert_eq!(
        reopened
            .turn_cancel_request(&address)
            .await
            .expect("read reopened base acceptor")
            .expect("reopened base acceptor remains projected")
            .request,
        after_step,
    );
    let replay = reopened
        .commit_runtime_state(commit)
        .await
        .expect("replay refreshed final commit");
    assert_eq!(replay.turn_cancel_input_outcome.len(), 1);
}
