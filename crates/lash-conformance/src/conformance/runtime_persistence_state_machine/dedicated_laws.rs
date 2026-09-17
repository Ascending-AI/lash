use super::*;

pub(super) async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
{
    assert_on_fresh_store(make, seed, |store| async move {
        law_lease_exclusivity_and_claim_generation_fencing(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 1, |store| async move {
        law_claimed_work_settles_exactly_once(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 2, |store| async move {
        law_reclaim_mediates_supersession(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 3, |store| async move {
        claim_honesty::law_reclaimed_predecessor_rejection_survives_successor_head_advance(store)
            .await
    })
    .await?;
    assert_on_fresh_store(make, seed + 4, |store| async move {
        law_head_cas_serializes_competing_commits(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 5, |store| async move {
        interrupted_claim_laws::stale_settlement_cannot_damage_successor(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 6, |store| async move {
        law_selected_batch_out_of_order_never_loses_work(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 7, |store| async move {
        law_turn_inputs_apply_once_in_order(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 8, |store| async move {
        law_commit_atomicity_and_stale_head_non_mutation(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 9, |store| async move {
        law_checkpoint_refs_track_content(store).await
    })
    .await
}

async fn assert_on_fresh_store<F, Fut, Law, LawFut>(
    make: &F,
    seed: u64,
    law: Law,
) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
    Law: FnOnce(Arc<dyn RuntimePersistence>) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    // Structural guard: every dedicated law obtains its own backend here.
    law(make(seed).await.runtime).await
}

async fn law_lease_exclusivity_and_claim_generation_fencing(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    let ops = [
        RuntimePersistenceOp::ClaimLease { owner: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::ClaimLease { owner: 1 },
        RuntimePersistenceOp::Crash,
        RuntimePersistenceOp::ClaimLease { owner: 1 },
        RuntimePersistenceOp::RenewLease { stale: true },
        RuntimePersistenceOp::ClaimWorkWithStaleLease,
        RuntimePersistenceOp::ClaimTurnInputsWithStaleLease,
    ];
    for op in &ops {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 10, op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert!(
        shape[RunShapeCounter::LeaseFenceRejections] >= 3,
        "generation fencing did not reject stale renewal and claim attempts"
    );
    prop_assert_eq!(model.work.len(), 1, "stale claim attempt removed work");
    prop_assert_eq!(model.inputs.len(), 1, "stale claim attempt removed input");
    Ok(())
}

async fn law_claimed_work_settles_exactly_once(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(&session_id(), &owner, "claimed-work-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("selected work absent"))?;
    let mut state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
        .completing_queue_claim(claim.completion());
    let first = store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let replay = store
        .commit_runtime_state(commit)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        replay.head_revision,
        first.head_revision,
        "exact commit replay advanced the head twice"
    );
    prop_assert_eq!(
        &replay.checkpoint_ref,
        &first.checkpoint_ref,
        "exact commit replay returned a different receipt"
    );
    prop_assert!(
        store
            .list_queued_work(&session_id())
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "settled work remained live"
    );
    state.apply_persisted_commit_result(first);
    let (second_settlement, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:second-settlement"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let second = store
        .commit_runtime_state(second_settlement.completing_queue_claim(claim.completion()))
        .await;
    prop_assert!(
        matches!(second, Err(StoreError::QueuedWorkClaimSuperseded { .. })),
        "distinct second settlement was not rejected: {second:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before, "distinct second settlement")
        .await
        .map_err(TestCaseError::fail)?;
    Ok(())
}

async fn law_reclaim_mediates_supersession(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .try_claim_session_execution_lease(
            &session_id(),
            &stale_owner,
            "reclaim-stale-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_claim = store
        .claim_ready_queued_work(
            &session_id(),
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(4),
        )
        .await
        .map(crate::QueuedWorkClaimOutcome::claim)
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("coalesced work absent"))?;
    prop_assert_eq!(stale_claim.batches.len(), 2, "join claim did not coalesce");
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let successor_owner = owner(1);
    let successor_lease = store
        .try_claim_session_execution_lease(
            &session_id(),
            &successor_owner,
            "reclaim-successor-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;
    let before_partial_selection = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let partial_selection = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    prop_assert!(
        matches!(
            &partial_selection,
            Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) if required_batch_ids == &[first.batch_id.clone(), second.batch_id.clone()]
        ),
        "partial selection did not return the literal interrupted composition: {partial_selection:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before_partial_selection,
        "partial interrupted-composition selected claim",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &[first.batch_id.clone(), second.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("successor did not reclaim full composition"))?;

    let mut state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(31),
    ));
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_claim.completion()),
        )
        .await;
    prop_assert!(
        matches!(
            stale_result,
            Err(StoreError::QueuedWorkClaimSuperseded { .. })
        ),
        "mixed old/new ownership did not reject the whole stale completion: {stale_result:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before,
        "reclaim-mediated all-or-nothing rejection",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let pending_while_successor_holds = store
        .list_pending_queued_work(&session_id())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        pending_while_successor_holds.is_empty(),
        "the rejected predecessor commit disturbed successor ownership of the full composition"
    );
    prop_assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                &session_id(),
                &successor_lease.fence(),
                &successor_owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&first.batch_id),
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .acquired_no_rows(),
        "the rejected predecessor commit released the successor-owned batch"
    );
    store
        .release_session_execution_lease(&successor_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let pending = store
        .list_pending_queued_work(&session_id())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let pending_ids = pending
        .iter()
        .map(|batch| batch.batch_id.as_str())
        .collect::<BTreeSet<_>>();
    prop_assert_eq!(
        pending_ids,
        BTreeSet::from([first.batch_id.as_str(), second.batch_id.as_str()]),
        "rejected stale completion did not preserve both batches as pending"
    );
    prop_assert_eq!(successor_claim.batches.len(), 2);
    Ok(())
}

async fn law_head_cas_serializes_competing_commits(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let input = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .try_claim_session_execution_lease(
            &session_id(),
            &stale_owner,
            "head-cas-stale-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_work = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("queued work absent"))?;
    let stale_input = store
        .claim_next_turn_inputs(&session_id(), &stale_lease.fence(), &stale_owner, 1)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("turn input absent"))?;
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor_owner = owner(1);
    let _successor_lease = store
        .try_claim_session_execution_lease(
            &session_id(),
            &successor_owner,
            "head-cas-successor-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;

    let mut loser_state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    loser_state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(41),
    ));
    let (loser, _) = RuntimeCommit::persisted_state_for_test(&loser_state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-loser"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let loser = loser
        .releasing_session_execution_lease(stale_lease.completion())
        .completing_queue_claim(stale_work.completion())
        .completing_turn_input_claim(stale_input.completion());
    let mut winner_state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    winner_state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(42),
    ));
    let (winner, _) = RuntimeCommit::persisted_state_for_test(&winner_state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-winner"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let winner_result = store
        .commit_runtime_state(winner)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(winner_result.head_revision, 1);
    let before_loser = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let loser_result = store.commit_runtime_state(loser).await;
    prop_assert!(
        matches!(loser_result, Err(StoreError::HeadRevisionConflict { .. })),
        "competing CAS loser was not rejected: {loser_result:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before_loser, "head-CAS loser")
        .await
        .map_err(TestCaseError::fail)?;
    let snapshot = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    prop_assert_eq!(&snapshot["head"]["head_revision"], &serde_json::json!(1));
    prop_assert_eq!(snapshot["work"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["pending_work"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["pending_inputs"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["applications"].as_array().map(Vec::len), Some(0));
    prop_assert_eq!(&stale_input.inputs[0].input_id, &input.input_id);
    Ok(())
}

async fn law_selected_batch_out_of_order_never_loses_work(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let earlier = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let later = store
        .enqueue_queued_work(queued_draft(1, 1, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(&session_id(), &owner, "selected-batch-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&later.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("later batch absent"))?;
    let mut state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    state.apply_persisted_commit_result(result);
    let remaining = store
        .list_queued_work(&session_id())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(remaining.len(), 1);
    prop_assert_eq!(
        &remaining[0].batch_id,
        &earlier.batch_id,
        "settling batch 2 lost batch 1"
    );
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&earlier.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("earlier batch no longer claimable"))?;
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        store
            .list_queued_work(&session_id())
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty()
    );
    Ok(())
}

async fn law_turn_inputs_apply_once_in_order(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_pending_turn_input(turn_input_draft(1, 1))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(&session_id(), &owner, "turn-input-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let mut claim = store
        .claim_next_turn_inputs(&session_id(), &lease.fence(), &owner, 10)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("turn inputs absent"))?;
    prop_assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    claim.record_initial_turn_application(&crate::TurnId::from("ordered-turn"), "ordered-message");
    let expected = claim.applications.clone();
    let state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
        .completing_turn_input_claim(claim.completion());
    store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    store
        .commit_runtime_state(commit)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        store
            .list_turn_input_applications(&session_id())
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?,
        expected,
        "input applications were reordered or duplicated"
    );
    Ok(())
}

async fn law_commit_atomicity_and_stale_head_non_mutation(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for op in [
        RuntimePersistenceOp::ClaimLease { owner: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::ClaimWork {
            selected: false,
            selection: 0,
        },
        RuntimePersistenceOp::ClaimTurnInputs { max_inputs: 2 },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 7,
            settle_work: true,
            settle_inputs: true,
            stale_head: true,
        },
    ] {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 11, &op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert_eq!(model.head_revision, 0);
    prop_assert_eq!(model.work.len(), 1);
    prop_assert_eq!(model.inputs.len(), 1);
    prop_assert!(model.components.tool_ref.is_none());
    apply_operation(
        store.as_ref(),
        None,
        &mut model,
        &mut shape,
        11,
        &RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 7,
            settle_work: true,
            settle_inputs: true,
            stale_head: false,
        },
    )
    .await
    .map_err(TestCaseError::fail)?;
    prop_assert_eq!(model.head_revision, 1);
    prop_assert!(model.work.is_empty() && model.inputs.is_empty());
    prop_assert!(
        model.components.tool_ref.is_some()
            && model.components.plugin_ref.is_some()
            && model.components.execution_ref.is_some()
    );
    Ok(())
}

async fn law_checkpoint_refs_track_content(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for op in [
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 1,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 5,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
    ] {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 12, &op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert!(shape[RunShapeCounter::CheckpointStores] >= 9);
    prop_assert!(shape[RunShapeCounter::CheckpointRefReuses] >= 3);
    assert_model_agreement(store.as_ref(), &model)
        .await
        .map_err(TestCaseError::fail)
}
