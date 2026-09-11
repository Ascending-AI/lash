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
        .validate_turn_cancellation_binding(&address.session_id, fence, TURN_CANCEL_BINDING_ID)
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
    crate::TurnCancelClosureAuthorization::new(
        address.clone(),
        TURN_CANCEL_BINDING_ID,
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

/// The durable closure slot is non-overwritable, survives lease-generation
/// changes, preserves its admitted physical scope, and can be consumed only by
/// a current owner presenting the exact authorization.
pub(super) async fn turn_cancel_closure_authorization_is_fenced_and_non_overwritable(
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
    store
        .validate_turn_cancellation_binding(
            &request.session_id,
            &first.fence(),
            TURN_CANCEL_BINDING_ID,
        )
        .await
        .expect("bind the first authority");
    let turn = TurnId::from("turn-cancel-closure-authorization:first");
    let address = crate::TurnAddress::new(&request.session_id, &turn);
    let exact = closure_authorization(
        &address,
        crate::ExecutionScope::process("turn-cancel-shared-process"),
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
        address.execution_scope(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
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
                TURN_CANCEL_BINDING_ID,
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
        }) if session_id == &request.session_id
    ));
    assert!(matches!(
        store
            .pending_turn_cancel_closures(
                &request.session_id,
                &first.fence(),
                "different-turn-control-owner",
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
                TURN_CANCEL_BINDING_ID,
            )
            .await
            .expect("successor adopts pending authorization"),
        vec![exact.clone()]
    );
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
    stale_commit.turn_cancel_closure_authorization = Some(exact.clone());
    stale_commit.release_session_execution_lease = Some(first.completion());
    assert!(matches!(
        store.commit_runtime_state(stale_commit).await,
        Err(crate::StoreError::SessionExecutionLeaseExpired { .. })
            | Err(crate::StoreError::SessionExecutionLeaseRenewalRefused { .. })
    ));
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("stale commit retains the exact closure pin"),
        vec![exact.clone()]
    );
    store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &successor.fence(),
            &turn,
            &crate::TurnCancelIntentSnapshot::Absent,
            crate::TurnCancelRepairDecision::CancellationDidNotWin,
            Some(&exact),
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
                TURN_CANCEL_BINDING_ID,
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

    let second_address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-closure-authorization:second"),
    );
    let stale = closure_authorization(
        &second_address,
        second_address.execution_scope(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &first.fence(),
    );
    assert!(matches!(
        store
            .authorize_turn_cancel_closure(&first.fence(), &stale)
            .await,
        Err(crate::StoreError::SessionExecutionLeaseExpired { .. })
            | Err(crate::StoreError::SessionExecutionLeaseRenewalRefused { .. })
    ));
    let current = closure_authorization(
        &second_address,
        second_address.execution_scope(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &successor.fence(),
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&successor.fence(), &current)
            .await
            .expect("current successor authorizes after takeover"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &successor.fence(),
            &second_address.turn_id,
            &crate::TurnCancelIntentSnapshot::Absent,
            crate::TurnCancelRepairDecision::CancellationDidNotWin,
            Some(&current),
        )
        .await
        .expect("consume successor authorization")
        .into_applied()
        .expect("successor intent remains absent");
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
            commit.turn_cancel_closure_authorization = Some(closure_authorization.clone());
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
                    crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
                    Some(&closure_authorization),
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
                    crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
                    Some(&later_authorization),
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

/// Escalating a durable after-step request to an immediate abort upgrades the
/// row in place: the address keeps one record, the record carries the
/// stronger request, and a same-or-weaker request never downgrades it.
pub(super) async fn turn_cancel_request_escalation_upgrades_the_durable_record(
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
        .expect("escalate the durable request");
    let durable = store
        .turn_cancel_request(&address)
        .await
        .expect("read durable request")
        .expect("request persists");
    assert_eq!(
        durable.request, abort,
        "an immediate abort upgrades the after-step row in place"
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
        durable.request, abort,
        "a delayed base-gate projection cannot downgrade an accepted escalation"
    );

    // Exact header equality is not a sufficient CAS: project B, return to the
    // byte-identical A through a stronger ingress write, then prove that the
    // original A snapshot is still stale because its revision did not return.
    let original_a = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("snapshot A before the ABA schedule");
    assert!(
        store
            .reconcile_turn_cancel_winner(&address, &original_a, &stale_base)
            .await
            .expect("project B with current authority")
    );
    store
        .record_turn_cancel_request(abort.clone())
        .await
        .expect("return the request header from B to A");
    let current_a = store
        .turn_cancel_request_intent(&address)
        .await
        .expect("read A after ABA");
    assert_eq!(current_a.request(), original_a.request());
    assert_ne!(current_a, original_a, "ABA must advance intent freshness");
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
        durable.request, abort,
        "a weaker request never downgrades the upgraded row"
    );

    // Reopen models an owner crash after the upgrade: the upgraded record is
    // what the successor reads.
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
    assert_eq!(durable.request, abort);
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
                crate::TurnCancelRepairDecision::NoCancellationIntent,
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
                crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
                Some(&closure_authorization),
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
            crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&stronger_cancel)),
            Some(&closure_authorization),
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
            crate::TurnCancelRepairDecision::NoCancellationIntent,
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
                crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
                Some(&late_authorization),
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
            crate::TurnCancelRepairDecision::CancellationDidNotWin,
            Some(&completion_authorization),
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
    commit.turn_cancel_closure_authorization = Some(closure_authorization);

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
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("replay refreshed final commit");
    assert_eq!(replay.turn_cancel_input_outcome.len(), 1);
}
