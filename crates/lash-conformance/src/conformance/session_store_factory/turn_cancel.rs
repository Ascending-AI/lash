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
            let receipt = store
                .commit_runtime_state(commit)
                .await
                .expect("cancel final commit");
            assert_eq!(receipt.turn_cancel_input_outcome.len(), 1);
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
                    crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
                )
                .await
                .expect("repair the dead turn")
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
                .expect("read intent without historical payload reconstruction")
                .expect("winner intent remains retained");
            assert_eq!(intent, cancel);
            let repaired = reopened
                .repair_orphaned_active_turn_inputs(
                    &request.session_id,
                    &lease.fence(),
                    &turn_id,
                    crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&intent)),
                )
                .await
                .expect("repair later input from retained winner intent");
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
    .undelivered(crate::TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(cancel.clone())
        .await
        .expect("persist request before repair");
    let fence = lease(&store, &request.session_id, "intent-first-owner").await;
    assert!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &fence,
                &turn_id,
                crate::TurnCancelRepairDecision::NoCancellationIntent,
            )
            .await
            .expect("no-intent repair observes concurrent intent")
            .is_empty(),
        "durable intent must veto ordinary repair"
    );
    let pending = store
        .list_pending_turn_inputs(&request.session_id)
        .await
        .expect("read input after veto");
    assert_eq!(pending[0].state, crate::TurnInputState::PendingActive);
    let cancelled = store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &fence,
            &turn_id,
            crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
        )
        .await
        .expect("apply authoritative gate winner");
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
            crate::TurnCancelRepairDecision::NoCancellationIntent,
        )
        .await
        .expect("ordinary repair before request");
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
    assert!(
        store
            .repair_orphaned_active_turn_inputs(
                &request.session_id,
                &fence,
                &turn_id,
                crate::TurnCancelRepairDecision::CancellationWon(cancel_evidence(&cancel)),
            )
            .await
            .expect("late winner sees no targeted input")
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
    let repaired = store
        .repair_orphaned_active_turn_inputs(
            &request.session_id,
            &fence,
            &turn_id,
            crate::TurnCancelRepairDecision::CancellationDidNotWin,
        )
        .await
        .expect("apply completion gate decision");
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
