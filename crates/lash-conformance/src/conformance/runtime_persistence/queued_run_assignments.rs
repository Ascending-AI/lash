use super::*;
use lash_core::store::{BeginQueuedRun, QueuedRunRequest};

#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_checkpoint_assignment_survives_lane_rotation(
    store: Arc<dyn RuntimePersistence>,
) {
    use lash_core::store::{QueuedRunCommit, QueuedRunProgress, QueuedRunTerminal};
    let session_id = SessionId::from("queued-run-checkpoint-assignment");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "assigned"))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "dispose").await;
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: Some(crate::ExecutionScope::queue_drain(&session_id, "disposed")),
        request: QueuedRunRequest::Automatic,
        configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
        expected_head_revision: 0,
        initial_turn_index: 1,
    };
    let admission = store
        .begin_or_resume_queued_run(&lease.authority(), request.clone())
        .await
        .unwrap();
    let selected = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            1,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    let checkpoint_input = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &selected.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint assigned",
        ))
        .await
        .unwrap();
    let checkpoint_batch = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "checkpoint assigned",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .unwrap();
    let (checkpoint_inputs, checkpoint_batches) = store
        .claim_checkpoint_work(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &selected.admission.position.turn_id,
            crate::CheckpointKind::AfterWork,
            1,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    let checkpoint_inputs = checkpoint_inputs.unwrap();
    assert_eq!(
        checkpoint_inputs.inputs[0].input_id,
        checkpoint_input.input_id
    );
    assert_eq!(
        checkpoint_batches.unwrap().batches[0].batch_id,
        checkpoint_batch.batch_id
    );
    let single_input = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &selected.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "single checkpoint assignment",
        ))
        .await
        .unwrap();
    let single_claim = store
        .claim_active_turn_inputs(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &selected.admission.position.turn_id,
            crate::CheckpointKind::BeforeCompletion,
            1,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(single_claim.inputs[0].input_id, single_input.input_id);
    let assigned = store
        .pending_queued_run(&session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        assigned.revision, selected.admission.revision,
        "checkpoint assignment does not invalidate the pending physical commit revision"
    );
    assert_eq!(assigned.assigned_members.len(), 3);
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "host-abandon").await;
    let unrelated_batch = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "unrelated raw claim",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .unwrap();
    let unrelated = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &lease.authority(),
            &lease.owner,
            crate::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&unrelated_batch.batch_id),
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    assert!(unrelated.claim.is_some());
    let unrelated_turn_id = TurnId::from("unrelated-active-turn");
    let unrelated_input = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &unrelated_turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "unrelated active claim",
        ))
        .await
        .unwrap();
    let unrelated_input_claim = store
        .claim_active_turn_inputs(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &unrelated_turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unrelated_input_claim.inputs[0].input_id,
        unrelated_input.input_id
    );
    assert_eq!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .unwrap()
            .assigned_members,
        assigned.assigned_members,
        "a different physical turn cannot assign work to this run"
    );
    let later = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "unassigned"))
        .await
        .unwrap();
    let mut unrelated_advance = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "unrelated-advance"),
    );
    unrelated_advance.session_execution_lease_fence = Some(lease.authority());
    unrelated_advance.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: selected.admission.position.next(&admission.scope).unwrap(),
            members: vec![lash_core::store::QueuedRunMember::Batch(
                unrelated_batch.batch_id.clone(),
            )],
            withheld_members: Vec::new(),
            include_outbox: false,
        },
    }));
    assert!(
        store.commit_runtime_state(unrelated_advance).await.is_err(),
        "a raw current-generation claim cannot become this run's continuation"
    );
    let mut settlement = QueuedRunCommit {
        scope: admission.scope,
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Settle {
            terminal: QueuedRunTerminal::Empty,
        },
    };
    assert!(
        store
            .settle_queued_run(&lease.authority(), settlement.clone())
            .await
            .is_err(),
        "empty disposition cannot discard selected work"
    );
    assert!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .is_some(),
        "rejected disposition rolls back admission"
    );
    settlement.progress = QueuedRunProgress::Settle {
        terminal: QueuedRunTerminal::Failed {
            code: crate::RuntimeErrorCode::QueuedWork,
            message: "host abandoned this submission".into(),
        },
    };
    let settled = store
        .settle_queued_run(&lease.authority(), settlement.clone())
        .await
        .unwrap();
    assert!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .is_none()
    );
    let batches = store.list_queued_work(&session_id).await.unwrap();
    assert_eq!(
        batches
            .iter()
            .map(|batch| &batch.batch_id)
            .collect::<Vec<_>>(),
        vec![&unrelated_batch.batch_id],
        "abandon after lane rotation removes checkpoint assignment and preserves unrelated live claims"
    );
    for claim in [&checkpoint_inputs, &single_claim, &unrelated_input_claim] {
        store.abandon_turn_input_claim(claim).await.unwrap();
    }
    let inputs = store.list_pending_turn_inputs(&session_id).await.unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|input| &input.input.input_id)
            .collect::<Vec<_>>(),
        vec![&unrelated_input.input_id, &later.input_id],
        "checkpoint assignment is terminalized without runtime redrive"
    );
    let next = store
        .claim_next_turn_inputs(&session_id, &lease.authority(), &lease.owner, 64)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.inputs
            .iter()
            .map(|input| &input.input_id)
            .collect::<Vec<_>>(),
        vec![&later.input_id],
        "only unassigned work remains executable"
    );
    settlement.expected_revision = u64::MAX;
    assert_eq!(
        store
            .settle_queued_run(&lease.authority(), settlement)
            .await
            .unwrap()
            .revision,
        settled.revision,
        "matching disposition receipt precedes revision checking"
    );
    let pending = store.list_pending_turn_inputs(&session_id).await.unwrap();
    assert_eq!(
        pending.len(),
        2,
        "old disposition replay retains the later input"
    );
    assert_eq!(pending[0].input.input_id, unrelated_input.input_id);
    assert_eq!(pending[1].input.input_id, later.input_id);
    assert!(
        matches!(
            pending[1].status,
            crate::PendingTurnInputReadStatus::Held { .. }
        ),
        "old disposition replay retains the later live claim"
    );
    let receipt = store
        .begin_or_resume_queued_run(&lease.authority(), request)
        .await
        .unwrap();
    assert_eq!(receipt.terminal, settled.terminal);
}

/// A resumed run retakes the open rows its checkpoints were assigned under
/// the resuming generation, even from a peer that took one in between
/// (FIG-3552). Ownership moves only through the claim CAS: the retaken rows
/// come back beside the members, never as run input.
#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_resume_retakes_its_open_checkpoint_assignments(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("queued-run-resume-retakes-assignments");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let member = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "member"))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "first").await;
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: Some(crate::ExecutionScope::queue_drain(&session_id, "retakes")),
        request: QueuedRunRequest::Automatic,
        configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
        expected_head_revision: 0,
        initial_turn_index: 1,
    };
    let admission = store
        .begin_or_resume_queued_run(&lease.authority(), request.clone())
        .await
        .unwrap();
    let selected = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            1,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    assert!(selected.reacquired_inputs.is_empty() && selected.reacquired_queued.is_empty());
    let checkpoint_input = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &selected.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint assigned",
        ))
        .await
        .unwrap();
    let checkpoint_batch = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "checkpoint assigned",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .unwrap();
    let (checkpoint_inputs, checkpoint_batches) = store
        .claim_checkpoint_work(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &selected.admission.position.turn_id,
            crate::CheckpointKind::AfterWork,
            1,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    let checkpoint_inputs = checkpoint_inputs.unwrap();
    let checkpoint_batches = checkpoint_batches.unwrap();
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();

    // A peer takes the assigned batch under its own generation and dies.
    let peer = claim_session_execution_lease_for_test(&store, &session_id, "peer").await;
    let peer_claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id,
            &peer.authority(),
            &peer.owner,
            crate::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&checkpoint_batch.batch_id),
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap()
        .claim
        .unwrap();
    store
        .release_session_execution_lease(&peer.authority())
        .await
        .unwrap();

    let successor = claim_session_execution_lease_for_test(&store, &session_id, "successor").await;
    let resumed = store
        .select_queued_run(
            &successor.authority(),
            &admission.scope,
            &successor.owner,
            1,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap();
    let generation = successor.authority().fencing_token;
    assert_eq!(
        resumed
            .inputs
            .iter()
            .flat_map(|claim| claim.inputs.iter().map(|input| input.input_id.clone()))
            .collect::<Vec<_>>(),
        vec![member.input_id],
        "the retaken assignments are not run input"
    );
    assert_eq!(resumed.reacquired_inputs.len(), 1);
    let retaken_input = &resumed.reacquired_inputs[0];
    assert_eq!(retaken_input.session_lease_generation, generation);
    assert_ne!(retaken_input.claim_id, checkpoint_inputs.claim_id);
    assert_eq!(retaken_input.inputs[0].input_id, checkpoint_input.input_id);
    assert_eq!(resumed.reacquired_queued.len(), 1);
    let retaken_batch = &resumed.reacquired_queued[0];
    assert_eq!(retaken_batch.session_lease_generation, generation);
    assert_ne!(retaken_batch.claim_id, checkpoint_batches.claim_id);
    assert_ne!(retaken_batch.claim_id, peer_claim.claim_id);
    assert_eq!(retaken_batch.batches[0].batch_id, checkpoint_batch.batch_id);
}
