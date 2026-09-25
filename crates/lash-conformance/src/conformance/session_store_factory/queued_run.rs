use super::*;
use lash_core::store::{
    BeginQueuedRun, QueuedRunCommit, QueuedRunProgress, QueuedRunRequest, QueuedRunTerminal,
};
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes an empty persisted admission"
)]
pub async fn session_store_factory_discovers_empty_pending_queued_run(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("queued-run-empty-discovery"),
        "queued-run-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session");
    let state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
        &store,
        &request.session_id,
        "empty-run-owner",
    )
    .await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: request.session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: crate::RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            },
        )
        .await
        .expect("admit empty run");
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
        .expect("freeze empty selection");
    assert_eq!(selected.admission.members, Some(Vec::new()));
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .expect("release old owner");
    drop(store);
    assert_eq!(
        factory
            .has_claimable_queued_work(&request, 100)
            .await
            .expect("discover pending admission"),
        Some(true),
        "a frozen empty pending run must wake its replacement worker"
    );
    let reopened = factory
        .open_existing_store(&request)
        .await
        .expect("open session")
        .expect("session exists");
    let successor = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
        &reopened,
        &request.session_id,
        "empty-run-successor",
    )
    .await;
    reopened
        .settle_queued_run(
            &successor.authority(),
            QueuedRunCommit {
                scope: admission.scope.clone(),
                expected_revision: selected.admission.revision,
                progress: QueuedRunProgress::Settle {
                    terminal: QueuedRunTerminal::Empty,
                },
            },
        )
        .await
        .expect("settle frozen empty run");
    assert_eq!(
        factory
            .has_claimable_queued_work(&request, 100)
            .await
            .expect("discover settled run"),
        Some(false),
        "settled receipts do not keep the scheduler awake"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture commits one checkpoint input retained only by admission assignment"
)]
pub async fn session_store_factory_retains_assigned_input_tombstone(
    factory: Arc<dyn crate::store::ConformanceSessionStoreFactory>,
) {
    let request = session_store_request(
        &SessionId::from("queued-run-assigned-retention"),
        "queued-run-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create retention session");
    let state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("initial"),
        ))
        .await
        .expect("enqueue initial input");
    let lease = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
        &store,
        &request.session_id,
        "assigned-retention-owner",
    )
    .await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: request.session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: crate::RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
            },
        )
        .await
        .expect("admit run");
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
        .expect("select initial input");
    let checkpoint_draft = crate::PendingTurnInputDraft::new(
        &request.session_id,
        crate::TurnInputIngress::active_turn(
            &selected.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
        ),
        crate::TurnInput::text("checkpoint retention"),
    )
    .with_source_key("assigned-only");
    let checkpoint = store
        .enqueue_pending_turn_input(checkpoint_draft.clone())
        .await
        .expect("enqueue checkpoint");
    let claim = store
        .claim_active_turn_inputs(
            &request.session_id,
            &lease.authority(),
            &lease.owner,
            &selected.admission.position.turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("claim checkpoint")
        .expect("checkpoint input");
    let mut commit = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "checkpoint-retention"),
    );
    commit.session_execution_lease_fence = Some(lease.authority());
    commit.completed_turn_input_claims.push(claim.completion());
    commit.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: selected
                .admission
                .position
                .next(&admission.scope)
                .expect("next position"),
            members: selected.admission.members.expect("initial selection"),
            withheld_members: Vec::new(),
            include_outbox: false,
        },
    }));
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit checkpoint input");
    let pending = store
        .pending_queued_run(&request.session_id)
        .await
        .expect("read admission")
        .expect("pending run");
    let member = crate::store::QueuedRunMember::Input(checkpoint.input_id.clone());
    assert!(pending.assigned_members.contains(&member));
    assert!(
        !pending
            .initial_members
            .iter()
            .flatten()
            .chain(pending.members.iter().flatten())
            .chain(pending.withheld_members.iter())
            .any(|value| value == &member),
        "only assignment retains the completed checkpoint input"
    );
    let report = store.vacuum().await.expect("vacuum pending run");
    assert_eq!(
        report.removed_pending_turn_input_tombstone_count, 0,
        "vacuum must retain assigned-only input tombstones while their run is pending"
    );
    let replay = store
        .enqueue_pending_turn_input(checkpoint_draft)
        .await
        .expect("replay input source key");
    assert_eq!(replay.input_id, checkpoint.input_id);
    assert_eq!(
        replay.state.kind(),
        crate::TurnInputStateKind::Completed,
        "vacuum must not make completed assigned work executable again"
    );
}
