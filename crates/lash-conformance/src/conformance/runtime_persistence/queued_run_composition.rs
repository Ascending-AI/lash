use super::*;
use lash_core::store::{BeginQueuedRun, QueuedRunMember, QueuedRunRequest};

pub async fn queued_run_selected_excludes_pending_input(store: Arc<dyn RuntimePersistence>) {
    assert_composition(store, "queued-run-selected-composition", false).await;
}

pub async fn queued_run_automatic_prefers_pending_input(store: Arc<dyn RuntimePersistence>) {
    assert_composition(store, "queued-run-automatic-composition", true).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture must establish both pending work kinds"
)]
async fn assert_composition(store: Arc<dyn RuntimePersistence>, session: &str, automatic: bool) {
    let session_id = SessionId::from(session);
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "pending input"))
        .await
        .expect("enqueue pending input");
    let batch = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "queued batch",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue batch");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "composition").await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: if automatic {
                    QueuedRunRequest::Automatic
                } else {
                    QueuedRunRequest::Selected {
                        batch_ids: vec![batch.batch_id.clone()],
                    }
                },
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            },
        )
        .await
        .expect("admit composition");
    let selected = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("freeze composition");
    let expected = if automatic {
        QueuedRunMember::Input(input.input_id)
    } else {
        QueuedRunMember::Batch(batch.batch_id)
    };
    assert_eq!(
        selected.admission.members,
        Some(vec![expected]),
        "selected drains contain only requested batches; automatic drains choose pending inputs before batches"
    );
    assert_eq!(selected.inputs.is_empty(), !automatic);
    assert_eq!(selected.queued.is_empty(), automatic);
    assert!(selected.already_satisfied.is_empty());
    let unassigned = if automatic {
        store
            .claim_ready_queued_work(
                &session_id,
                &lease.authority(),
                &lease.owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("unselected batch remains claimable")
            .claim()
            .is_some()
    } else {
        store
            .claim_next_turn_inputs(&session_id, &lease.authority(), &lease.owner, 64)
            .await
            .expect("unselected input remains claimable")
            .is_some()
    };
    assert!(
        unassigned,
        "selection must leave the other work kind unclaimed"
    );
}
