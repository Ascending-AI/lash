use std::sync::Arc;

use super::session_store_request;

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
    for disposition in [
        crate::TurnCancelDisposition::Defer,
        crate::TurnCancelDisposition::Drop,
    ] {
        for path in [
            RepairPath::Commit,
            RepairPath::Teardown,
            RepairPath::CrashBeforeRepair,
        ] {
            turn_cancel_disposition_crash_cell(Arc::clone(&factory), disposition, path).await;
        }
    }

    async fn turn_cancel_disposition_crash_cell(
        factory: Arc<dyn crate::SessionStoreFactory>,
        disposition: crate::TurnCancelDisposition,
        path: RepairPath,
    ) {
        let suffix = format!("{:?}-{:?}", disposition, path).to_ascii_lowercase();
        let request = session_store_request(
            &format!("turn-cancel-{suffix}"),
            "turn-cancel-drop-model",
            crate::SessionRelation::Root,
        );
        let turn_id = format!("turn-cancel-{suffix}:turn");
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
        .undelivered(disposition);
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
            let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
            commit.interrupted_turn_input_turn_id = Some(turn_id.clone());
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
                .defer_orphaned_active_turn_inputs(
                    &request.session_id,
                    &lease.fence(),
                    match path {
                        RepairPath::Teardown => crate::OrphanedTurnInputScope::Turn(&turn_id),
                        RepairPath::CrashBeforeRepair => {
                            crate::OrphanedTurnInputScope::LaneGeneration {
                                resumable_turn_id: None,
                            }
                        }
                        RepairPath::Commit => unreachable!(),
                    },
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
        assert_eq!(durable.request, cancel);
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
    }
}
