use super::*;
use lash_core::store::{BeginQueuedRun, QueuedRunRequest};

/// A scheduler retry reacquires physical authority without changing the run
/// admitted by the first attempt. A competing explicit request cannot steal it.
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_identity_survives_lane_rotation(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("queued-run-identity");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: None,
        request: QueuedRunRequest::Automatic,
        configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
        expected_head_revision: 0,
        initial_turn_index: 1,
    };
    let first_fence = claim_session_execution_lease_for_test(&store, &session_id, "first")
        .await
        .authority();
    let admitted = store
        .begin_or_resume_queued_run(&first_fence, request.clone())
        .await
        .expect("admit run");
    store
        .release_session_execution_lease(&first_fence)
        .await
        .expect("release physical owner");
    let next_fence = claim_session_execution_lease_for_test(&store, &session_id, "successor")
        .await
        .authority();
    let resumed = store
        .begin_or_resume_queued_run(&next_fence, request.clone())
        .await
        .expect("resume run");
    assert_eq!(
        serde_json::to_value(&admitted).unwrap(),
        serde_json::to_value(&resumed).unwrap(),
        "physical takeover retains the complete logical admission"
    );
    assert!(
        store
            .begin_or_resume_queued_run(&first_fence, request.clone())
            .await
            .is_err(),
        "stale owner cannot resume"
    );
    let competing = BeginQueuedRun {
        identity: Some(crate::ExecutionScope::queue_drain(&session_id, "different")),
        ..request
    };
    assert!(
        matches!(
            store
                .begin_or_resume_queued_run(&next_fence, competing)
                .await,
            Err(StoreError::QueuedRunConflict { .. })
        ),
        "an explicit request cannot alias active work"
    );
}

#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_selection_excludes_later_input_after_takeover(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("queued-run-selection");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let first = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "first"))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "first").await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
            },
        )
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
    let same_owner = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .unwrap();
    assert_eq!(
        same_owner.inputs.first().unwrap().claim_id,
        selected.inputs.first().unwrap().claim_id
    );
    assert_eq!(
        same_owner.inputs.first().unwrap().lease_token,
        selected.inputs.first().unwrap().lease_token,
        "retry under the same live lane reuses its physical claim"
    );
    assert_eq!(
        selected
            .inputs
            .into_iter()
            .next()
            .unwrap()
            .inputs
            .iter()
            .map(|input| &input.input_id)
            .collect::<Vec<_>>(),
        vec![&first.input_id]
    );
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "later"))
        .await
        .unwrap();
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();
    let successor = claim_session_execution_lease_for_test(&store, &session_id, "second").await;
    let resumed = store
        .select_queued_run(
            &successor.authority(),
            &admission.scope,
            &successor.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .unwrap();
    assert_eq!(
        resumed
            .inputs
            .into_iter()
            .next()
            .unwrap()
            .inputs
            .iter()
            .map(|input| &input.input_id)
            .collect::<Vec<_>>(),
        vec![&first.input_id],
        "changed limits and later arrivals cannot alter admitted work"
    );
}

#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_commit_receipt_precedes_revisions_but_not_lane_fence(
    store: Arc<dyn RuntimePersistence>,
) {
    use lash_core::store::{QueuedRunCommit, QueuedRunProgress, QueuedRunTerminal};
    let session_id = SessionId::from("queued-run-receipt");
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "receipt").await;
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: Some(crate::ExecutionScope::queue_drain(&session_id, "explicit")),
        request: QueuedRunRequest::Automatic,
        configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
        expected_head_revision: 0,
        initial_turn_index: 1,
    };
    let admitted = store
        .begin_or_resume_queued_run(&lease.authority(), request.clone())
        .await
        .unwrap();
    let operation = crate::OperationId::new(admitted.scope.clone(), "final");
    let mut commit =
        RuntimeCommit::persisted_state_with_operation_for_testing(&state, &[], operation);
    commit.session_execution_lease_fence = Some(lease.authority());
    commit.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admitted.scope.clone(),
        expected_revision: admitted.revision,
        progress: QueuedRunProgress::Settle {
            terminal: QueuedRunTerminal::Empty,
        },
    }));
    let receipt = store.commit_runtime_state(commit.clone()).await.unwrap();
    assert!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .is_none(),
        "physical commit settles the admission atomically"
    );
    state.head_revision = receipt.head_revision;
    let later = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(crate::ExecutionScope::runtime_operation("later"), "commit"),
    );
    let later_receipt = store.commit_runtime_state(later).await.unwrap();
    assert!(later_receipt.head_revision > receipt.head_revision);
    let replay = store.commit_runtime_state(commit.clone()).await.unwrap();
    assert_eq!(
        replay.head_revision, receipt.head_revision,
        "lost reply replay survives admission and head advance"
    );
    let mut conflicting = commit.clone();
    conflicting.queued_run.as_mut().unwrap().progress = QueuedRunProgress::Settle {
        terminal: QueuedRunTerminal::Failed {
            code: crate::RuntimeErrorCode::QueuedWork,
            message: "different result".into(),
        },
    };
    assert!(
        store.commit_runtime_state(conflicting).await.is_err(),
        "receipt cannot accept conflicting terminal evidence"
    );
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();
    let successor = claim_session_execution_lease_for_test(&store, &session_id, "successor").await;
    assert!(
        store.commit_runtime_state(commit.clone()).await.is_err(),
        "receipt cannot bypass a stale lane fence"
    );
    commit.session_execution_lease_fence = Some(successor.authority());
    assert_eq!(
        store
            .commit_runtime_state(commit)
            .await
            .unwrap()
            .head_revision,
        receipt.head_revision
    );
    let replayed = store
        .begin_or_resume_queued_run(&successor.authority(), request)
        .await
        .unwrap();
    assert!(
        matches!(replayed.terminal, Some(QueuedRunTerminal::Empty)),
        "explicit identity returns its terminal receipt"
    );
}

#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_terminal_disposition_preserves_unassigned_work(
    store: Arc<dyn RuntimePersistence>,
) {
    use lash_core::store::{QueuedRunCommit, QueuedRunProgress, QueuedRunTerminal};
    let session_id = SessionId::from("queued-run-disposition");
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
    let later = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "unassigned"))
        .await
        .unwrap();
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
        1,
        "old disposition replay retains the later input"
    );
    assert_eq!(pending[0].input.input_id, later.input_id);
    assert!(
        matches!(
            pending[0].status,
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

#[expect(
    clippy::unwrap_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_frozen_batches_survive_takeover_and_changed_limits(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("queued-run-batches");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let first = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "first",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "first").await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
            },
        )
        .await
        .unwrap();
    store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            1,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(1),
        )
        .await
        .unwrap()
        .queued
        .into_iter()
        .next()
        .unwrap();
    store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "later",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .unwrap();
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();
    let successor = claim_session_execution_lease_for_test(&store, &session_id, "second").await;
    let selected = store
        .select_queued_run(
            &successor.authority(),
            &admission.scope,
            &successor.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .unwrap();
    assert_eq!(
        selected
            .queued
            .into_iter()
            .next()
            .unwrap()
            .batches
            .iter()
            .map(|batch| &batch.batch_id)
            .collect::<Vec<_>>(),
        vec![&first.batch_id]
    );
}

#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_continuation_commits_outbox_and_retains_receipts(
    store: Arc<dyn RuntimePersistence>,
) {
    use lash_core::store::{
        QueuedRunCommit, QueuedRunMember, QueuedRunPosition, QueuedRunProgress, QueuedRunTerminal,
    };
    let session_id = SessionId::from("queued-run-continuation");
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let first = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "initial"))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "continuation").await;
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: Some(crate::ExecutionScope::queue_drain(&session_id, "original")),
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
    let mut advance = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "physical-0"),
    );
    advance.session_execution_lease_fence = Some(lease.authority());
    advance
        .completed_turn_input_claims
        .push(selected.inputs.into_iter().next().unwrap().completion());
    advance
        .enqueued_queue_batches
        .push(checkpoint_claims::queued_draft(
            &session_id,
            "outbox",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ));
    let position = QueuedRunPosition {
        physical_ordinal: 1,
        turn_index: 2,
        turn_id: format!("{}:agent-frame:1", admission.scope.id()).into(),
    };
    advance.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: position.clone(),
            members: Vec::new(),
            withheld_members: Vec::new(),
            include_outbox: true,
        },
    }));
    let mut invalid_position = advance.clone();
    if let QueuedRunProgress::Advance { position, .. } =
        &mut invalid_position.queued_run.as_mut().unwrap().progress
    {
        position.turn_id = "unrelated-physical-turn".into();
    }
    assert!(
        store.commit_runtime_state(invalid_position).await.is_err(),
        "physical position must retain the canonical admitted turn identity"
    );
    let receipt = store.commit_runtime_state(advance.clone()).await.unwrap();
    let pending = store
        .pending_queued_run(&session_id)
        .await
        .unwrap()
        .expect("committed continuation stays discoverable after initial work settles");
    let batch_id = receipt.enqueued_queue_batches[0].batch_id.clone();
    assert_eq!(pending.position, position);
    assert_eq!(
        pending.initial_members,
        Some(vec![QueuedRunMember::Input(first.input_id)])
    );
    assert_eq!(
        pending.members,
        Some(vec![QueuedRunMember::Batch(batch_id.clone())]),
        "outbox IDs publish atomically with admission progress"
    );
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .unwrap();
    let successor = claim_session_execution_lease_for_test(&store, &session_id, "successor").await;
    let selected = store
        .select_queued_run(
            &successor.authority(),
            &admission.scope,
            &successor.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .unwrap();
    assert!(selected.inputs.is_empty());
    assert_eq!(
        selected.queued.first().unwrap().batches[0].batch_id,
        batch_id
    );
    state.head_revision = receipt.head_revision;
    let mut settle = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "physical-1"),
    );
    settle.session_execution_lease_fence = Some(successor.authority());
    settle
        .completed_queue_claims
        .push(selected.queued.into_iter().next().unwrap().completion());
    settle.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Settle {
            terminal: QueuedRunTerminal::Empty,
        },
    }));
    let terminal_receipt = store.commit_runtime_state(settle).await.unwrap();
    let newer = store
        .begin_or_resume_queued_run(
            &successor.authority(),
            BeginQueuedRun {
                identity: None,
                expected_head_revision: terminal_receipt.head_revision,
                ..request.clone()
            },
        )
        .await
        .unwrap();
    assert_ne!(newer.scope, admission.scope);
    advance.session_execution_lease_fence = Some(successor.authority());
    let replay = store.commit_runtime_state(advance).await.unwrap();
    assert_eq!(
        replay.head_revision, receipt.head_revision,
        "prior physical receipt precedes newer admission ownership"
    );
    assert_eq!(
        store
            .begin_or_resume_queued_run(&successor.authority(), request)
            .await
            .unwrap()
            .terminal,
        Some(QueuedRunTerminal::Empty)
    );
    assert_eq!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .unwrap()
            .scope,
        newer.scope,
        "old receipt does not change the pending run"
    );
}

#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_exact_selection_never_commits_a_partial_claim(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("queued-run-exact");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let first = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "first",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .unwrap();
    let second = store
        .enqueue_queued_work(checkpoint_claims::queued_draft(
            &session_id,
            "second",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .unwrap();
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "exact").await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Selected {
                    batch_ids: vec![first.batch_id.clone(), second.batch_id.clone()],
                },
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
            },
        )
        .await
        .unwrap();
    for limit in [1, 64] {
        assert!(
            store
                .select_queued_run(
                    &lease.authority(),
                    &admission.scope,
                    &lease.owner,
                    limit,
                    &admission.configuration,
                    lash_core::testing::queued_work_claim_policy(limit)
                )
                .await
                .is_err(),
            "incompatible exact batches cannot admit a prefix regardless of row limit"
        );
        assert!(
            store
                .pending_queued_run(&session_id)
                .await
                .unwrap()
                .unwrap()
                .members
                .is_none(),
            "rejected exact claim rolls back selection"
        );
    }
    store
        .cancel_queued_work_batch(&session_id, &second.batch_id)
        .await
        .unwrap()
        .expect("second batch remains unclaimed after refusal");
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
        .unwrap();
    assert_eq!(selected.already_satisfied, vec![second.batch_id.clone()]);
    for takeover in [false, true] {
        let retry_lease = if takeover {
            store
                .release_session_execution_lease(&lease.authority())
                .await
                .unwrap();
            claim_session_execution_lease_for_test(&store, &session_id, "exact-successor").await
        } else {
            lease.clone()
        };
        let replay = store
            .select_queued_run(
                &retry_lease.authority(),
                &admission.scope,
                &retry_lease.owner,
                1,
                &admission.configuration,
                lash_core::testing::queued_work_claim_policy(1),
            )
            .await
            .unwrap();
        assert_eq!(
            replay.already_satisfied,
            vec![second.batch_id.clone()],
            "frozen retry retains requested batches already absent at initial selection"
        );
        assert_eq!(replay.queued[0].batches[0].batch_id, first.batch_id);
        if takeover {
            let mut advance = RuntimeCommit::persisted_state_with_operation_for_testing(
                &state,
                &[],
                crate::OperationId::new(admission.scope.clone(), "exact-physical-0"),
            );
            advance.session_execution_lease_fence = Some(retry_lease.authority());
            advance
                .completed_queue_claims
                .push(replay.queued[0].completion());
            advance.queued_run = Some(Box::new(lash_core::store::QueuedRunCommit {
                scope: admission.scope.clone(),
                expected_revision: replay.admission.revision,
                progress: lash_core::store::QueuedRunProgress::Advance {
                    position: replay.admission.position.next(&admission.scope).unwrap(),
                    members: Vec::new(),
                    withheld_members: Vec::new(),
                    include_outbox: false,
                },
            }));
            store.commit_runtime_state(advance).await.unwrap();
            let continued = store
                .select_queued_run(
                    &retry_lease.authority(),
                    &admission.scope,
                    &retry_lease.owner,
                    1,
                    &admission.configuration,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await
                .unwrap();
            assert_eq!(
                continued.already_satisfied,
                vec![second.batch_id.clone()],
                "a batch consumed by this run never becomes an already-satisfied request"
            );
        }
    }
}

#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "conformance fixtures fail at the violated durable invariant"
)]
pub async fn queued_run_advance_rejects_unassigned_members_but_keeps_checkpoint_claims(
    store: Arc<dyn RuntimePersistence>,
) {
    use lash_core::store::{
        QueuedRunCommit, QueuedRunMember, QueuedRunPosition, QueuedRunProgress,
    };
    let session_id = SessionId::from("queued-run-provenance");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "provenance").await;
    let admission = store
        .begin_or_resume_queued_run(
            &lease.authority(),
            BeginQueuedRun {
                session_id: session_id.clone(),
                identity: None,
                request: QueuedRunRequest::Automatic,
                configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
                expected_head_revision: 0,
                initial_turn_index: 1,
            },
        )
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
    let later = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &selected.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint-only",
        ))
        .await
        .unwrap();
    let mut advance = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "physical-0"),
    );
    advance.session_execution_lease_fence = Some(lease.authority());
    advance.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: selected.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: QueuedRunPosition {
                physical_ordinal: 1,
                turn_index: 2,
                turn_id: format!("{}:agent-frame:1", admission.scope.id()).into(),
            },
            members: vec![QueuedRunMember::Input(later.input_id.clone())],
            withheld_members: Vec::new(),
            include_outbox: false,
        },
    }));
    assert!(
        store.commit_runtime_state(advance.clone()).await.is_err(),
        "Advance cannot assign an unclaimed arrival"
    );
    assert_eq!(
        store
            .pending_queued_run(&session_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        selected.admission.revision,
        "provenance refusal rolls back admission"
    );
    let checkpoint_claim = store
        .claim_active_turn_inputs(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &selected.admission.position.turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .commit_runtime_state(advance)
        .await
        .expect("current-lane checkpoint claim is valid continuation evidence");
    let replay = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .unwrap();
    assert_eq!(
        replay.inputs.into_iter().next().unwrap().lease_token,
        checkpoint_claim.lease_token
    );
    let second = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &replay.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint-second",
        ))
        .await
        .unwrap();
    let withheld = store
        .enqueue_pending_turn_input(checkpoint_claims::pending_active_turn_input_draft(
            &session_id,
            &replay.admission.position.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint-withheld",
        ))
        .await
        .unwrap();
    let second_claim = store
        .claim_active_turn_inputs(
            &session_id,
            &lease.authority(),
            &lease.owner,
            &replay.admission.position.turn_id,
            crate::CheckpointKind::AfterWork,
            2,
        )
        .await
        .unwrap()
        .unwrap();
    let mut batches = Vec::new();
    let mut batch_claims = Vec::new();
    for name in ["checkpoint-frame-one", "checkpoint-frame-two"] {
        batches.push(
            store
                .enqueue_queued_work(checkpoint_claims::queued_draft(
                    &session_id,
                    name,
                    DeliveryPolicy::EarliestSafeBoundary,
                ))
                .await
                .unwrap(),
        );
        batch_claims.push(
            store
                .claim_checkpoint_work(
                    &session_id,
                    &lease.authority(),
                    &lease.owner,
                    &replay.admission.position.turn_id,
                    crate::CheckpointKind::AfterWork,
                    0,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await
                .unwrap()
                .1
                .unwrap(),
        );
    }
    let mut next = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "physical-1"),
    );
    next.expected_head_revision = 1;
    next.session_execution_lease_fence = Some(lease.authority());
    let current = vec![
        QueuedRunMember::Input(second.input_id.clone()),
        QueuedRunMember::Input(later.input_id.clone()),
    ];
    next.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: replay.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: replay.admission.position.next(&admission.scope).unwrap(),
            members: current.clone(),
            withheld_members: vec![
                QueuedRunMember::Input(withheld.input_id.clone()),
                QueuedRunMember::Batch(batches[0].batch_id.clone()),
                QueuedRunMember::Batch(batches[1].batch_id.clone()),
            ],
            include_outbox: false,
        },
    }));
    store.commit_runtime_state(next).await.unwrap();
    let grouped = store
        .select_queued_run(
            &lease.authority(),
            &admission.scope,
            &lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("a recorded continuation can retain multiple checkpoint claim tokens");
    assert_eq!(grouped.admission.members, Some(current));
    assert_eq!(grouped.inputs.len(), 2);
    assert_eq!(grouped.inputs[0].claim_id, second_claim.claim_id);
    assert_eq!(grouped.inputs[0].lease_token, second_claim.lease_token);
    assert_eq!(
        grouped.inputs[0]
            .inputs
            .iter()
            .map(|input| &input.input_id)
            .collect::<Vec<_>>(),
        vec![&second.input_id],
        "current subset excludes the withheld row sharing its token"
    );
    assert_eq!(grouped.inputs[1].claim_id, checkpoint_claim.claim_id);
    assert_eq!(grouped.inputs[1].lease_token, checkpoint_claim.lease_token);
    let joined = vec![
        QueuedRunMember::Batch(batches[1].batch_id.clone()),
        QueuedRunMember::Input(withheld.input_id.clone()),
        QueuedRunMember::Batch(batches[0].batch_id.clone()),
        QueuedRunMember::Input(second.input_id.clone()),
        QueuedRunMember::Input(later.input_id.clone()),
    ];
    let mut rejoin = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(admission.scope.clone(), "physical-2"),
    );
    rejoin.expected_head_revision = 2;
    rejoin.session_execution_lease_fence = Some(lease.authority());
    rejoin.queued_run = Some(Box::new(QueuedRunCommit {
        scope: admission.scope.clone(),
        expected_revision: grouped.admission.revision,
        progress: QueuedRunProgress::Advance {
            position: grouped.admission.position.next(&admission.scope).unwrap(),
            members: joined.clone(),
            withheld_members: Vec::new(),
            include_outbox: false,
        },
    }));
    store.commit_runtime_state(rejoin).await.unwrap();
    let replay = store
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
    assert_eq!(replay.admission.members, Some(joined));
    assert_eq!(
        replay
            .inputs
            .iter()
            .flat_map(|claim| claim.inputs.iter().map(|input| &input.input_id))
            .collect::<Vec<_>>(),
        vec![&withheld.input_id, &second.input_id, &later.input_id]
    );
    for (id, expected) in [
        (&withheld.input_id, &second_claim),
        (&second.input_id, &second_claim),
        (&later.input_id, &checkpoint_claim),
    ] {
        let claim = replay
            .inputs
            .iter()
            .find(|claim| claim.inputs.iter().any(|input| input.input_id == id))
            .unwrap();
        assert_eq!(claim.claim_id, expected.claim_id);
        assert_eq!(claim.lease_token, expected.lease_token);
    }
    assert_eq!(
        replay
            .queued
            .iter()
            .flat_map(|claim| claim.batches.iter().map(|batch| &batch.batch_id))
            .collect::<Vec<_>>(),
        vec![&batches[1].batch_id, &batches[0].batch_id]
    );
    for (id, expected) in [
        (&batches[1].batch_id, &batch_claims[1]),
        (&batches[0].batch_id, &batch_claims[0]),
    ] {
        let claim = replay
            .queued
            .iter()
            .find(|claim| claim.batches.iter().any(|batch| batch.batch_id == id))
            .unwrap();
        assert_eq!(claim.claim_id, expected.claim_id);
        assert_eq!(claim.lease_token, expected.lease_token);
    }
}
