use super::*;

pub(super) async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
{
    assert_on_fresh_store(make, seed, |store| async move {
        law_stale_fences_admit_nothing(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 1, |store| async move {
        law_admitted_work_settles_exactly_once(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 2, |store| async move {
        law_a_resumed_root_keeps_its_admission_across_fences(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 4, |store| async move {
        law_head_cas_serializes_competing_commits(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 5, |store| async move {
        law_stale_settlement_cannot_damage_successor(store).await
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
    Law: FnOnce(Arc<dyn RuntimeStore>) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    // Structural guard: every dedicated law obtains its own backend here.
    law(make(seed).await.runtime).await
}

fn fail(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

async fn seal(store: &Arc<dyn RuntimeStore>, index: u8) -> Result<DriveFence, TestCaseError> {
    store
        .seal_drive_epoch_for_test(
            &session_id(),
            &owner(index),
            "dedicated-law-executor",
            60_000,
        )
        .await
        .map_err(fail)?
        .acquired()
        .ok_or_else(|| fail("the drive epoch seal lost a concurrent admission"))
}

async fn admit(
    store: &Arc<dyn RuntimeStore>,
    fence: &DriveFence,
    root: &str,
    head: AdmittedHead,
) -> Result<RootAdmission, TestCaseError> {
    store
        .admit_root(&admission_request(fence, &TurnId::from(root), head, 64))
        .await
        .map_err(fail)?
        .ok_or_else(|| fail("the admission missed its head"))
}

fn completing(root: &str, admission: &RootAdmission) -> IngressSettlement {
    let mut settlement = IngressSettlement::new(TurnId::from(root));
    if let Some(queued) = &admission.queued {
        settlement.completed_batches.push(queued.completion());
    }
    if let Some(inputs) = &admission.inputs {
        settlement.completed_inputs.push(inputs.completion());
    }
    settlement
}

/// `commit` settling `settlement` under `fence`, ending its root.
fn final_commit(
    mut commit: RuntimeCommit,
    fence: &DriveFence,
    settlement: IngressSettlement,
) -> RuntimeCommit {
    let root = settlement.root.clone();
    commit.drive_fence = Some(Box::new(fence.clone()));
    commit.ingress = Some(settlement);
    commit.root_terminal = Some(Box::new(crate::store::RootTerminalWrite {
        commit: crate::store::TurnCommitId::new(root.clone(), 0),
        turn: root.clone(),
        root,
        stop: None,
    }));
    commit
}

fn state_with_tool_generation(generation: u64) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(generation),
    ));
    state
}

/// N4 through the model: a superseded fence admits neither family.
async fn law_stale_fences_admit_nothing(store: Arc<dyn RuntimeStore>) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    let ops = [
        RuntimePersistenceOp::SealFence { owner: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::SealFence { owner: 1 },
        RuntimePersistenceOp::Crash,
        RuntimePersistenceOp::SealFence { owner: 1 },
        RuntimePersistenceOp::AdmitWorkWithStaleFence,
        RuntimePersistenceOp::AdmitTurnInputsWithStaleFence,
    ];
    for op in &ops {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 10, op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert!(
        shape[RunShapeCounter::StaleFenceRejections] >= 2,
        "the drive fence did not refuse both stale admissions"
    );
    prop_assert_eq!(model.work.len(), 1, "a stale admission removed work");
    prop_assert_eq!(model.inputs.len(), 1, "a stale admission removed input");
    Ok(())
}

/// A root's final commit settles its rows once: the exact replay answers
/// from its receipt, and a distinct second settlement of the same rows is
/// refused because no root holds them any more.
async fn law_admitted_work_settles_exactly_once(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(fail)?;
    let fence = seal(&store, 0).await?;
    let admission = admit(
        &store,
        &fence,
        "settles-once",
        AdmittedHead::Batch(batch.batch_id),
    )
    .await?;
    let mut state = state_with_tool_generation(0);
    let commit = final_commit(
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        &fence,
        completing("settles-once", &admission),
    );
    let first = store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(fail)?;
    let replay = store.commit_runtime_state(commit).await.map_err(fail)?;
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
            .map_err(fail)?
            .is_empty(),
        "settled work remained live"
    );
    state.apply_persisted_commit_result(first);
    let (second_settlement, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:second-settlement"),
            "commit",
        ))
        .map_err(fail)?;
    let mut second_settlement = second_settlement;
    second_settlement.drive_fence = Some(Box::new(fence.clone()));
    second_settlement.ingress = Some(completing("settles-once", &admission));
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let second = store.commit_runtime_state(second_settlement).await;
    prop_assert!(
        matches!(second, Err(StoreError::IngressRowNotAdmitted { .. })),
        "distinct second settlement was not rejected: {second:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before, "distinct second settlement")
        .await
        .map_err(TestCaseError::fail)?;
    Ok(())
}

/// N3: a coalesced admission read back under a successor's fence is the
/// recorded one, the predecessor's settlement is refused whole without
/// disturbing the successor's rows, and the successor settles them.
async fn law_a_resumed_root_keeps_its_admission_across_fences(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(fail)?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(fail)?;
    let predecessor = seal(&store, 0).await?;
    let head = AdmittedHead::Batch(first.batch_id.clone());
    let admitted = admit(&store, &predecessor, "resumed", head.clone()).await?;
    prop_assert_eq!(
        admitted.batch_ids().len(),
        2,
        "the joined admission did not coalesce"
    );
    store
        .supersede_drive_epoch_for_test(&predecessor)
        .await
        .map_err(fail)?;
    let successor = seal(&store, 1).await?;
    let resumed = admit(&store, &successor, "resumed", head).await?;
    prop_assert_eq!(
        json(&resumed).map_err(TestCaseError::fail)?,
        json(&admitted).map_err(TestCaseError::fail)?,
        "the successor must read the recorded admission back"
    );

    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(final_commit(
            RuntimeCommit::persisted_state_for_test(&state_with_tool_generation(31), &[]),
            &predecessor,
            completing("resumed", &admitted),
        ))
        .await;
    prop_assert!(
        matches!(stale_result, Err(StoreError::StaleDriveFence { .. })),
        "the predecessor's settlement was not refused whole: {stale_result:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before, "superseded predecessor settlement")
        .await
        .map_err(TestCaseError::fail)?;
    prop_assert!(
        store
            .list_open_queued_work(&session_id())
            .await
            .map_err(fail)?
            .is_empty(),
        "the refused settlement released the root's rows"
    );
    store
        .commit_runtime_state(final_commit(
            RuntimeCommit::persisted_state_for_test(&state_with_tool_generation(32), &[]),
            &successor,
            completing("resumed", &resumed),
        ))
        .await
        .map_err(fail)?;
    prop_assert!(
        store
            .list_queued_work(&session_id())
            .await
            .map_err(fail)?
            .is_empty(),
        "the successor could not settle both rows"
    );
    let _ = second;
    Ok(())
}

/// Head CAS serializes competing commits: a loser carrying the root's
/// settlement under the live fence but a stale head is refused whole, the
/// winner's head standing and the root's rows still bound.
async fn law_head_cas_serializes_competing_commits(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let input = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(fail)?;
    store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(fail)?;
    let fence = seal(&store, 0).await?;
    let admission = admit(
        &store,
        &fence,
        "cas",
        AdmittedHead::Input(input.input_id.clone()),
    )
    .await?;

    let (loser, _) = RuntimeCommit::persisted_state_for_test(&state_with_tool_generation(41), &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-loser"),
            "commit",
        ))
        .map_err(fail)?;
    let loser = final_commit(loser, &fence, completing("cas", &admission));
    let (winner, _) = RuntimeCommit::persisted_state_for_test(&state_with_tool_generation(42), &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-winner"),
            "commit",
        ))
        .map_err(fail)?;
    let winner_result = store.commit_runtime_state(winner).await.map_err(fail)?;
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
    prop_assert_eq!(admission.input_ids(), vec![input.input_id]);
    Ok(())
}

/// A superseded predecessor's settlement of a subset of the rows is refused
/// whole and cannot disturb the successor that resumed the root; the
/// successor still settles every row.
async fn law_stale_settlement_cannot_damage_successor(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(fail)?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(fail)?;
    let predecessor = seal(&store, 0).await?;
    let head = AdmittedHead::Batch(first.batch_id.clone());
    let admitted = admit(&store, &predecessor, "damage", head.clone()).await?;
    store
        .supersede_drive_epoch_for_test(&predecessor)
        .await
        .map_err(fail)?;
    let successor = seal(&store, 1).await?;
    let resumed = admit(&store, &successor, "damage", head).await?;

    let mut subset = IngressSettlement::new(TurnId::from("damage"));
    subset.completed_batches.push(crate::QueuedWorkCompletion {
        session_id: session_id(),
        batch_ids: vec![second.batch_id.clone()],
    });
    let state = state_with_tool_generation(51);
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(final_commit(
            RuntimeCommit::persisted_state_for_test(&state, &[]),
            &predecessor,
            subset,
        ))
        .await;
    prop_assert!(
        matches!(stale_result, Err(StoreError::StaleDriveFence { .. })),
        "stale subset settlement was not refused: {stale_result:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before,
        "stale subset settlement after the successor resumed",
    )
    .await
    .map_err(TestCaseError::fail)?;
    prop_assert_eq!(
        store
            .list_queued_work(&session_id())
            .await
            .map_err(fail)?
            .len(),
        2
    );
    prop_assert!(
        store
            .list_open_queued_work(&session_id())
            .await
            .map_err(fail)?
            .is_empty(),
        "stale subset settlement released the successor's rows"
    );
    prop_assert!(
        seal(&store, 2).await.is_ok(),
        "a stale settlement must not prevent a successor drive seal"
    );
    let live = seal(&store, 3).await?;
    store
        .commit_runtime_state(final_commit(
            RuntimeCommit::persisted_state_for_test(&state, &[]),
            &live,
            completing("damage", &resumed),
        ))
        .await
        .map_err(fail)?;
    prop_assert!(
        store
            .list_queued_work(&session_id())
            .await
            .map_err(fail)?
            .is_empty(),
        "the successor could not settle the root's rows"
    );
    let _ = admitted;
    Ok(())
}

async fn law_turn_inputs_apply_once_in_order(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(fail)?;
    let second = store
        .enqueue_pending_turn_input(turn_input_draft(1, 1))
        .await
        .map_err(fail)?;
    let fence = seal(&store, 0).await?;
    let admission = admit(
        &store,
        &fence,
        "ordered-turn",
        AdmittedHead::Input(first.input_id.clone()),
    )
    .await?;
    prop_assert_eq!(
        admission.input_ids(),
        vec![first.input_id.clone(), second.input_id.clone()]
    );
    let mut inputs = *admission
        .inputs
        .clone()
        .ok_or_else(|| fail("the root admitted no inputs"))?;
    inputs.record_initial_turn_application(&TurnId::from("ordered-turn"), "ordered-message");
    let expected = inputs.applications.clone();
    let mut settlement = IngressSettlement::new(TurnId::from("ordered-turn"));
    settlement.completed_inputs.push(inputs.completion());
    let state = RuntimeSessionState {
        session_id: session_id(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = final_commit(
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        &fence,
        settlement,
    );
    store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(fail)?;
    store.commit_runtime_state(commit).await.map_err(fail)?;
    prop_assert_eq!(
        store
            .list_turn_input_applications(&session_id())
            .await
            .map_err(fail)?,
        expected,
        "input applications were reordered or duplicated"
    );
    Ok(())
}

async fn law_commit_atomicity_and_stale_head_non_mutation(
    store: Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for op in [
        RuntimePersistenceOp::SealFence { owner: 0 },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::AdmitTurnInputs { max_inputs: 2 },
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
    prop_assert!(
        model.inputs.is_empty() && model.work.len() == 1 && model.root.is_none(),
        "the root's final commit settles its input and leaves the open batch"
    );
    prop_assert!(
        model.components.tool_ref.is_some()
            && model.components.plugin_ref.is_some()
            && model.components.execution_ref.is_some()
    );
    Ok(())
}

async fn law_checkpoint_refs_track_content(
    store: Arc<dyn RuntimeStore>,
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
