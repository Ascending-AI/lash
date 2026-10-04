//! Exact closure receipts, successor fences and authorization consumption.

use super::*;
use pretty_assertions::assert_eq;

/// A successful closure has an admitted Run and its physical final operation.
async fn commit_admitted_teardown(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &crate::store::ShiftFence,
    turn: &TurnId,
    observed: &crate::TurnCancelIntentSnapshot,
    settlement: &crate::TurnCancelClosureSettlement,
) -> Result<crate::TurnCancelInputOutcome, crate::StoreError> {
    let (commit, _) = teardown_commit(store.as_ref(), fence, turn, observed, settlement)
        .await?
        .with_operation(crate::OperationId::turn(fence.session(), turn, "final"))?;
    let commit = crate::conformance::prepare_final_commit(store, commit).await;
    store
        .commit_runtime_state(commit)
        .await
        .map(|receipt| receipt.turn_cancel_input_outcome)
}

/// ADR 0105 §9: a stored commit's exact replay under a superseded fence
/// answers its receipt without writing, including a retained copy of its own
/// settled cancellation closure. `snapshot` serializes every durable table.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_stale_fence_receipt_replay_leaves_the_store_byte_identical<F, Fut>(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    snapshot: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    let request = session_store_request(
        &SessionId::from("stale-fence-receipt-no-writes"),
        "stale-fence-receipt-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");
    let address = crate::TurnAddress::new(&request.session_id, TurnId::from("settled-turn"));
    let first = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("receipt-first", "receipt-first:1"),
            "receipt-first:executor",
            60_000,
        )
        .await
        .expect("seal the original shift")
        .acquired()
        .expect("the original shift is free");
    let authorization = authorize_closure(
        store.store(),
        &first,
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &address.turn_id,
            "final",
        ))
        .expect("stamp the final commit");
    commit.shift_fence = Some(Box::new(first.clone()));
    commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
        settlement: settled_closure(&authorization, None),
        observed_intent: crate::TurnCancelIntentSnapshot::Absent,
        admitted_intent: None,
    });
    let commit = crate::conformance::prepare_final_commit(store.store(), commit).await;
    let receipt = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit and settle the original closure");
    assert!(!receipt.receipt_replayed);
    assert!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the settled closure slot")
            .is_empty()
    );

    // A duplicate authorization can arrive after its commit consumed the
    // slot. Keep that exact artifact present so a replay's cleanup is an
    // observable write rather than a DELETE that happens to match no rows.
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first, &authorization)
            .await
            .expect("retain the duplicate settled authorization"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    store
        .store()
        .supersede_shift_epoch_for_test(&first)
        .await
        .expect("supersede the original shift");
    let successor = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("receipt-successor", "receipt-successor:1"),
            "receipt-successor:executor",
            60_000,
        )
        .await
        .expect("seal a successor shift")
        .acquired()
        .expect("the successor shift is free");
    assert!(successor.epoch() > first.epoch());
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the retained closure before replay"),
        vec![authorization.clone()]
    );
    let before = snapshot().await;
    assert!(!before.is_empty(), "the snapshot must cover durable tables");
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("the stale fence's exact replay answers its receipt");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, receipt.head_revision);
    assert_eq!(replay.checkpoint_ref, receipt.checkpoint_ref);
    let after = snapshot().await;
    assert_eq!(after.len(), before.len(), "replay changed the table set");
    for ((table, before), (after_table, after)) in before.into_iter().zip(after) {
        assert_eq!(after_table, table, "replay changed the table set");
        assert_eq!(after, before, "stale-fence receipt replay wrote to {table}");
    }
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the retained closure after replay"),
        vec![authorization]
    );
}

/// Replaying an already committed receipt must not consume a newer pending
/// authorization that happens to reuse the same turn address.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(in crate::conformance) async fn turn_cancel_exact_replay_preserves_different_pending_authorization(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-exact-replay-preserves-new-authorization"),
        "turn-cancel-exact-replay-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");
    let address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-exact-replay:turn"),
    );
    let first = store
        .store()
        .seal_shift_epoch_for_test(
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
        store.store(),
        &first,
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &address.turn_id,
            "final",
        ))
        .expect("stamp exact replay operation");
    commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
        settlement: settled_closure(&first_authorization, None),
        observed_intent: crate::TurnCancelIntentSnapshot::Absent,
        admitted_intent: None,
    });
    commit.shift_fence = Some(Box::new(first.clone()));
    let commit = crate::conformance::prepare_final_commit(store.store(), commit).await;
    store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit first authorization");

    let successor = store
        .store()
        .seal_shift_epoch_for_test(
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
        store.store(),
        &successor,
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(in crate::conformance) async fn turn_cancel_closure_settlement_is_fenced_and_non_overwritable(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-closure-authorization"),
        "turn-cancel-closure-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");
    let first = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("closure-first", "closure-first:incarnation"),
            "closure-first:executor",
            60_000,
        )
        .await
        .expect("claim first closure lane")
        .acquired()
        .expect("first closure lane is free");
    let physical_scope =
        crate::ExecutionScope::process(crate::ProcessId::fixture("turn-cancel-shared-process"));
    let selected_binding =
        crate::turn_control_binding_id_for_scope(TURN_CANCEL_BINDING_ID, &physical_scope)
            .expect("bind process scope");
    store
        .validate_turn_cancellation_binding(&first, &selected_binding, &physical_scope)
        .await
        .expect("bind the first authority");
    let turn = TurnId::from("turn-cancel-closure-authorization:first");
    let address = crate::TurnAddress::new(&request.session_id, &turn);
    let exact = closure_authorization(
        &address,
        physical_scope.clone(),
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &first,
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first, &exact)
            .await
            .expect("authorize vacant slot"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first, &exact)
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
            undelivered: crate::TurnCancelUndeliveredInputPolicy::Defer,
            mode: crate::TurnCancelMode::Immediate,
            honoured_after_step: None,
        }),
        &first,
    );
    assert!(matches!(
        store
            .authorize_turn_cancel_closure(&first, &conflicting)
            .await,
        Err(crate::StoreError::TurnCancelClosureConflict { .. })
    ));
    assert_eq!(
        store
            .pending_turn_cancel_closures(&first, &selected_binding, &physical_scope,)
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
                &first,
                &crate::turn_control_binding_id_for_scope(
                    TURN_CANCEL_BINDING_ID,
                    &crate::ExecutionScope::process(crate::ProcessId::fixture(
                        "turn-cancel-wrong-successor-process"
                    )),
                )
                .expect("bind wrong successor physical scope"),
                &crate::ExecutionScope::process(crate::ProcessId::fixture(
                    "turn-cancel-wrong-successor-process"
                )),
            )
            .await,
        Err(crate::StoreError::TurnCancelBindingMismatch { .. })
    ));
    assert!(matches!(
        store
            .pending_turn_cancel_closures(
                &first,
                &selected_binding,
                &crate::ExecutionScope::process(crate::ProcessId::fixture(
                    "turn-cancel-wrong-successor-process"
                )),
            )
            .await,
        Err(crate::StoreError::TurnCancelBindingMismatch { .. })
    ));

    let (final_commit, _) = teardown_commit(
        store.store().as_ref(),
        &first,
        &turn,
        &crate::TurnCancelIntentSnapshot::Absent,
        &settled_closure(&exact, None),
    )
    .await
    .expect("construct the admitted closure envelope")
    .with_operation(crate::OperationId::turn(
        &request.session_id,
        &turn,
        "final",
    ))
    .expect("stamp the physical turn's closure");
    let prepared = crate::conformance::prepare_final_commit(store.store(), final_commit).await;

    store
        .store()
        .supersede_shift_epoch_for_test(&first)
        .await
        .expect("release first owner");
    let successor = store
        .store()
        .seal_shift_epoch_for_test(
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
            .pending_turn_cancel_closures(&successor, &selected_binding, &physical_scope,)
            .await
            .expect("successor adopts pending authorization"),
        vec![exact.clone()]
    );
    let mut wrong_terminal = prepared.clone();
    wrong_terminal.shift_fence = Some(Box::new(successor.clone()));
    wrong_terminal
        .interrupted_turn
        .as_mut()
        .expect("the admitted final carries its closure")
        .settlement = settled_closure(&conflicting, None);
    assert!(matches!(
        store.commit_runtime_state(wrong_terminal).await,
        Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch { .. })
    ));
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("wrong terminal cannot consume the authorization"),
        vec![exact.clone()]
    );
    commit_admitted_teardown(
        store.store(),
        &successor,
        &turn,
        &crate::TurnCancelIntentSnapshot::Absent,
        &settled_closure(&exact, None),
    )
    .await
    .expect("current successor consumes exact authorization");
    assert!(
        store
            .pending_turn_cancel_closures(&successor, &selected_binding, &physical_scope,)
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

    // Consuming the exact authorization is the durable fence. Its authorizing
    // shift epoch alone cannot veto final settlement under ADR 0039.
    let stale_state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    let (mut stale_commit, _) = crate::RuntimeCommit::persisted_state_for_test(&stale_state)
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &turn,
            "stale-closure-final",
        ))
        .expect("stamp stale closure commit operation");
    stale_commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
        settlement: settled_closure(&exact, None),
        observed_intent: crate::TurnCancelIntentSnapshot::Absent,
        admitted_intent: None,
    });
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
        &first,
    );
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first, &stale)
            .await
            .expect("advisory takeover does not fence closure authorization"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    let current = closure_authorization(
        &second_address,
        physical_scope,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
        &successor,
    );
    assert!(matches!(
        store
            .authorize_turn_cancel_closure(&successor, &current)
            .await,
        Err(crate::StoreError::TurnCancelClosureConflict { .. })
    ));
    commit_admitted_teardown(
        store.store(),
        &successor,
        &second_address.turn_id,
        &crate::TurnCancelIntentSnapshot::Absent,
        &settled_closure(&stale, None),
    )
    .await
    .expect("consume successor authorization");

    // A session authority is stable across turns, while each authorization
    // retains the exact turn address recovered from durable input state.
    let session_request = session_store_request(
        &SessionId::from("turn-cancel-session-scope-variation"),
        "turn-cancel-session-scope-model",
        crate::SessionRelation::Root,
    );
    let session_store = factory
        .admit_view(&session_request)
        .await
        .expect("create session-scoped store");
    let session_lease = session_store
        .store()
        .seal_shift_epoch_for_test(
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
            &session_lease,
        )
        .is_err()
    );
    for address in [&first_session_address, &second_session_address] {
        session_store
            .validate_turn_cancellation_binding(
                &session_lease,
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
            &session_lease,
        );
        assert_eq!(authorization.admitted_scope(), &address.execution_scope());
        session_store
            .authorize_turn_cancel_closure(&session_lease, &authorization)
            .await
            .expect("authorize exact session turn");
        commit_admitted_teardown(
            session_store.store(),
            &session_lease,
            &address.turn_id,
            &crate::TurnCancelIntentSnapshot::Absent,
            &settled_closure(&authorization, None),
        )
        .await
        .expect("consume exact session-turn authorization");
    }
}
