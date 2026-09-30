use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION;
use lash_core::store::IngressSettlement;
use lash_core::testing::RuntimeStoreTestDriveExt as _;
use pretty_assertions::assert_eq;

/// A backend must mint refs for checkpoint bodies and resolve those refs after
/// both the ref-only successor write and the final read reopen the substrate.
///
/// This is the standing regression for the checkpoint-component failure
/// shape: write bodies, drop the writer, write only their refs through an
/// independently constructed handle, drop that writer, then construct a third
/// handle and hydrate. The helper owns construction order so a caller cannot
/// prebuild nominally cold handles before the writes they verify.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn complete_runtime_checkpoint_component_set_survives_cold_reopens<F>(make: F)
where
    F: Fn() -> Arc<dyn RuntimeStore>,
{
    let open = make();
    let open_identity = Arc::downgrade(&open);
    admit_conformance_session(&open, &SessionId::from("checkpoint-component-refs")).await;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("checkpoint-component-refs"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_execution_state_snapshot(Some(b"known-execution-state".to_vec().into()));
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    first_commit.checkpoint.components.extend([
        (
            "arbitrary/unchanged".to_string(),
            crate::HydratedCheckpointComponent::changed(b"stable-body".to_vec()),
        ),
        (
            "arbitrary/duplicate-ref".to_string(),
            crate::HydratedCheckpointComponent::changed(b"stable-body".to_vec()),
        ),
        (
            "arbitrary/deleted".to_string(),
            crate::HydratedCheckpointComponent::changed(b"delete-me".to_vec()),
        ),
        (
            "arbitrary/changed".to_string(),
            crate::HydratedCheckpointComponent::changed(b"before".to_vec()),
        ),
    ]);
    let first =
        commit_runtime_state_for_test(&open, first_commit, "checkpoint-component-refs-first")
            .await
            .expect("commit checkpoint component bodies");
    let unchanged_descriptor = first.manifest.components["arbitrary/unchanged"].clone();
    let changed_before = first.manifest.components["arbitrary/changed"].clone();
    assert_eq!(
        unchanged_descriptor.blob_ref.as_str(),
        crate::BlobRef::for_content(b"stable-body").0
    );

    drop(open);

    let reopen = make();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopen)),
        "checkpoint-component reopen factory reused the writer handle"
    );
    let reopen_identity = Arc::downgrade(&reopen);
    admit_conformance_session(&reopen, &SessionId::from("checkpoint-component-refs")).await;

    // Exercise the production hydration and ordinary commit boundary. No test
    // code re-inserts arbitrary keys: the runtime-owned complete set must carry
    // them as unchanged refs.
    state = crate::conformance::helpers::load_window_state(
        &reopen,
        &SessionId::from("checkpoint-component-refs"),
    )
    .await
    .expect("hydrate resident checkpoint component set")
    .expect("seeded checkpoint state");
    let mut ordinary_turn_projection = state.to_snapshot();
    ordinary_turn_projection.turn_index += 1;
    state.adopt_snapshot(ordinary_turn_projection.clone());
    let second_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let carried = second_commit
        .checkpoint
        .components
        .get("arbitrary/unchanged")
        .expect("ordinary commit carries unknown key");
    assert_eq!(carried.body(), None, "unknown component must ride ref-only");
    assert_eq!(carried.blob_ref(), Some(&unchanged_descriptor.blob_ref));
    let measured = second_commit
        .measure_budget()
        .expect("measure ordinary complete-set commit");
    let root_bytes = rmp_serde::to_vec_named(
        &second_commit
            .checkpoint
            .manifest(crate::FleetFormat::current())
            .expect("project ordinary checkpoint root"),
    )
    .expect("encode ordinary checkpoint root")
    .len();
    assert_eq!(
        measured.checkpoint_bytes, root_bytes,
        "unchanged carried refs must add no component-body bytes to the budget"
    );
    let second =
        commit_runtime_state_for_test(&reopen, second_commit, "checkpoint-component-refs-second")
            .await
            .expect("commit unchanged checkpoint component refs");
    assert_eq!(
        second.manifest.components["arbitrary/unchanged"], unchanged_descriptor,
        "an unchanged arbitrary component must commit ref-only and reuse its descriptor"
    );
    assert!(
        second.manifest.components.contains_key("arbitrary/deleted"),
        "ordinary commits must retain every unknown component"
    );
    state = crate::conformance::helpers::load_window_state(
        &reopen,
        &SessionId::from("checkpoint-component-refs"),
    )
    .await
    .expect("hydrate state after ordinary complete-set commit")
    .expect("ordinary complete-set checkpoint state");

    // Explicit owner mutation still uses absence from the complete listing as
    // deletion. The arbitrary store-law mutations remain direct because the
    // runtime intentionally has no typed owner for those keys.
    state.set_execution_state_snapshot(None);
    let mut third_commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    third_commit
        .checkpoint
        .components
        .remove("arbitrary/deleted");
    third_commit.checkpoint.components.insert(
        "arbitrary/changed".to_string(),
        crate::HydratedCheckpointComponent::changed(b"after".to_vec()),
    );
    let third =
        commit_runtime_state_for_test(&reopen, third_commit, "checkpoint-component-refs-third")
            .await
            .expect("commit explicit known and arbitrary component mutations");
    assert_ne!(
        third.manifest.components["arbitrary/changed"].blob_ref, changed_before.blob_ref,
        "a changed arbitrary component must mint a new content ref"
    );
    assert!(
        !third.manifest.components.contains_key("arbitrary/deleted"),
        "absence from the complete key listing is deletion"
    );
    assert!(
        !third
            .manifest
            .components
            .contains_key(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        "explicit deletion of a known component must remove it"
    );
    drop(reopen);

    let cold_reopen = make();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&cold_reopen))
            && !std::sync::Weak::ptr_eq(&reopen_identity, &Arc::downgrade(&cold_reopen)),
        "checkpoint-component cold reader reused a writer handle"
    );
    admit_conformance_session(&cold_reopen, &SessionId::from("checkpoint-component-refs")).await;
    let read = cold_reopen
        .load_session_window(
            &SessionId::from("checkpoint-component-refs"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("cold-load refs-only checkpoint")
        .expect("refs-only checkpoint session");
    let checkpoint = read.checkpoint.expect("hydrated refs-only checkpoint");
    assert_eq!(
        checkpoint.component_body("arbitrary/unchanged"),
        Some(&b"stable-body"[..]),
        "hydrate -> ordinary commit -> hydrate must preserve unknown bytes exactly"
    );
    assert_eq!(
        checkpoint.component_body("arbitrary/duplicate-ref"),
        Some(&b"stable-body"[..]),
        "two component keys may resolve the same deduplicated body"
    );
    assert_eq!(
        checkpoint.components["arbitrary/duplicate-ref"].blob_ref(),
        checkpoint.components["arbitrary/unchanged"].blob_ref(),
        "duplicate refs must resolve once and hydrate every owning key"
    );
    assert_eq!(
        checkpoint.components["arbitrary/unchanged"].blob_ref(),
        Some(&unchanged_descriptor.blob_ref),
        "round-trip must preserve the unknown component's content hash"
    );
    assert_eq!(
        checkpoint.component_body("arbitrary/changed"),
        Some(&b"after"[..])
    );
    assert!(!checkpoint.components.contains_key("arbitrary/deleted"));
    assert!(
        !checkpoint
            .components
            .contains_key(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        "known component deletion must survive cold hydration"
    );

    state = crate::conformance::helpers::load_window_state(
        &cold_reopen,
        &SessionId::from("checkpoint-component-refs"),
    )
    .await
    .expect("reload current state before rejection laws")
    .expect("current checkpoint state before rejection laws");
    let mut unknown = RuntimeCommit::persisted_state_for_test(&state, &[]);
    unknown.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("never-stored".to_string()),
                encoding_version: crate::FleetFormat::current().writer_version(
                    lash_core::surface_format!(CHECKPOINT_COMPONENT_ENCODING_VERSION),
                ),
            },
        },
    );
    let _rejection_lease = seal_drive_fence_for_test(
        &cold_reopen,
        &SessionId::from("checkpoint-component-refs"),
        "checkpoint-component-rejections",
    )
    .await;
    let unknown_error = cold_reopen
        .commit_runtime_state(unknown)
        .await
        .expect_err("arbitrary unknown ref must fail");
    assert!(matches!(
        unknown_error,
        StoreError::CheckpointComponentMissing { ref key, .. }
            if key == "arbitrary/unknown-ref"
    ));

    let mut mismatch = RuntimeCommit::persisted_state_for_test(&state, &[]);
    mismatch.checkpoint.components.insert(
        "arbitrary/versioned".to_string(),
        crate::HydratedCheckpointComponent::Changed {
            encoding_version: crate::FleetFormat::current().writer_version(
                lash_core::surface_format!(CHECKPOINT_COMPONENT_ENCODING_VERSION),
            ) + 1,
            body_ref: crate::store::BlobRef::for_content(b"unsupported"),
            body: b"unsupported".as_slice().into(),
        },
    );
    let mismatch_error = cold_reopen
        .commit_runtime_state(mismatch)
        .await
        .expect_err("arbitrary encoding-version mismatch must fail");
    assert!(matches!(
        &mismatch_error,
        StoreError::CheckpointComponentEncodingVersionMismatch { key, .. }
            if key == "arbitrary/versioned"
    ));
    assert!(
        mismatch_error
            .to_string()
            .contains("remedy: drain affected sessions and recreate the store"),
        "typed mismatch must name the operator remedy: {mismatch_error}"
    );
}

/// A ref-only checkpoint commit is valid only when every referenced component
/// already exists in the backend.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_rejects_unknown_component_ref(store: Arc<dyn RuntimeStore>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("checkpoint-unknown-ref"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("checkpoint-component-that-was-never-stored".to_string()),
                encoding_version: crate::FleetFormat::current().writer_version(
                    lash_core::surface_format!(CHECKPOINT_COMPONENT_ENCODING_VERSION),
                ),
            },
        },
    );

    let error = commit_runtime_state_for_test(&store, commit, "checkpoint-unknown-ref")
        .await
        .expect_err("a checkpoint must reject a ref whose body is absent");
    assert!(matches!(
        &error,
        StoreError::CheckpointComponentMissing { key, blob_ref }
            if key == "arbitrary/unknown-ref"
                && blob_ref.as_str() == "checkpoint-component-that-was-never-stored"
    ));
    assert!(
        error
            .to_string()
            .contains("checkpoint-component-that-was-never-stored"),
        "missing-component error must identify the unresolved ref: {error}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_leaf_without_frame_open_ancestor(store: Arc<dyn RuntimeStore>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("missing-frame-root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let node = SessionNodeRecord {
        node_id: "unframed-root".into(),
        parent_node_id: None,
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                ProtocolEvent::typed("unframed", serde_json::Value::Null).expect("protocol event"),
            ),
        },
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend { nodes: vec![node] },
        &[],
    );
    let expected_leaf_node_id = commit
        .graph
        .leaf_node_id()
        .cloned()
        .expect("derived unframed leaf");

    let error = store
        .commit_runtime_state(commit)
        .await
        .expect_err("every root graph must open with FrameOpen");

    assert!(matches!(
        error,
        StoreError::MissingFrameOpenAncestor { leaf_node_id }
            if leaf_node_id == expected_leaf_node_id
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn turn_input_application_identity_survives_pending_tombstone_vacuum(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = "turn-input-application";
    let fence = seal_drive_fence_for_test(
        &store,
        &SessionId::from(session_id),
        "turn-input-application-owner",
    )
    .await;
    let mut state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut expected = Vec::new();
    let mut replay = None;

    for (turn_index, turn_id) in ["z-first-application-turn", "a-second-application-turn"]
        .into_iter()
        .enumerate()
    {
        let enqueued = futures_util::future::try_join_all((0..2).map(|input_index| {
            store.enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    &SessionId::from(session_id),
                    &format!("canonical application {turn_index}:{input_index}"),
                )
                .with_source_key(format!(
                    "host:application-source-{turn_index}-{input_index}"
                )),
            )
        }))
        .await
        .expect("enqueue application inputs");
        let head = enqueued
            .iter()
            .min_by_key(|input| input.enqueue_seq)
            .expect("two inputs were enqueued");
        let admission = admitted_root(
            &store,
            &fence,
            turn_id,
            lash_core::store::AdmittedHead::Input(head.input_id.clone()),
        )
        .await;
        let mut admitted = *admission
            .inputs
            .clone()
            .expect("the root admits its inputs");
        let committed_message_id = format!("application-message-{turn_index}");
        admitted
            .record_initial_turn_application(&crate::TurnId::from(turn_id), &committed_message_id);
        let turn_expected = admitted.applications.clone();
        assert_eq!(enqueued.len(), turn_expected.len());

        let mut settlement = IngressSettlement::new(TurnId::from(turn_id));
        settlement.completed_inputs.push(admitted.completion());
        let mut commit = final_commit(
            RuntimeCommit::persisted_state_for_test(&state, &[]),
            &fence,
            settlement,
        );
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            session_id, turn_id, "final",
        ));
        if turn_index == 1 {
            replay = Some(commit.clone());
        }
        let result = store
            .commit_runtime_state(commit)
            .await
            .expect("commit application identity");
        state.head_revision = result.head_revision;

        assert_eq!(result.turn_input_applications, turn_expected);
        expected.extend(turn_expected);
    }

    let replayed = store
        .commit_runtime_state(replay.expect("second turn commit replay"))
        .await
        .expect("replay application turn commit");
    assert_eq!(
        replayed.turn_input_applications,
        expected[2..],
        "an exact turn-commit replay must retain its applications"
    );
    assert_eq!(
        store
            .list_turn_input_applications(&SessionId::from(session_id))
            .await
            .expect("read durable application identity"),
        expected,
        "applications must follow monotonic turn-commit order and must not double-count a replay"
    );

    store
        .vacuum(&SessionId::from(session_id))
        .await
        .expect("vacuum application tombstone");
    assert_eq!(
        store
            .list_turn_input_applications(&SessionId::from(session_id))
            .await
            .expect("read application identity after tombstone vacuum"),
        expected,
        "application reconciliation must come from the committed turn, not a pending snapshot"
    );
}

fn admitted_input_ids(admission: &lash_core::store::CheckpointAdmission) -> Vec<String> {
    admission
        .inputs
        .iter()
        .flat_map(|inputs| inputs.inputs.iter())
        .map(|input| input.input_id.as_str().to_string())
        .collect()
}

fn admitted_batch_ids(admission: &lash_core::store::CheckpointAdmission) -> Vec<String> {
    admission
        .queued
        .iter()
        .flat_map(|queued| queued.batches.iter())
        .map(|batch| batch.batch_id.as_str().to_string())
        .collect()
}

/// Admit what `turn`'s checkpoint `kind` takes at `step`, `turn` being its
/// own root.
async fn at_checkpoint(
    store: &Arc<dyn RuntimeStore>,
    fence: &lash_core::store::DriveFence,
    turn: &TurnId,
    kind: crate::CheckpointKind,
    step: &str,
    max_inputs: usize,
    policy: crate::TurnLaneAdmissionPolicy,
) -> Result<lash_core::store::CheckpointAdmission, StoreError> {
    admit_at_checkpoint_for_test(store, fence, turn, turn, kind, step, max_inputs, policy).await
}

/// A checkpoint's admission takes both families in one transaction, binds
/// them to its root and step, and no other step takes them again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_admission_takes_both_families_once(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("checkpoint-work");
    let turn_id = crate::TurnId::from("checkpoint-turn");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint input",
        ))
        .await
        .expect("enqueue checkpoint input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            &session_id,
            "checkpoint queued work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let fence = seal_drive_fence_for_test(&store, &session_id, "checkpoint-owner").await;

    let admitted = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "checkpoint-turn:step:1",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("admit both checkpoint work families");
    assert_eq!(
        admitted_input_ids(&admitted),
        vec![input.input_id.to_string()]
    );
    assert_eq!(
        admitted_batch_ids(&admitted),
        vec![batch.batch_id.to_string()]
    );

    let later_step = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "checkpoint-turn:step:2",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("a later checkpoint of the same root");
    assert!(
        later_step.is_empty(),
        "rows a checkpoint admitted are bound to its step; no other step takes them"
    );
}

/// FIG-3976: a checkpoint-admitted input has no root binding while its
/// delivery is in flight; the commit that completes it binds it to the root
/// that applied it, so `root_of_input` resolves it by one point read. The
/// row's own point read, `pending_turn_input`, answers what the pending list
/// answers for it at each stage: open once enqueued, admitted to its root
/// from the checkpoint until the root's commit completes it (FIG-4044), and
/// absent from then on.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_checkpoint_applied_input_resolves_to_its_root_by_point_read(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("checkpoint-applied-binding");
    let turn = crate::TurnId::from("checkpoint-applied-binding:turn");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint applied input",
        ))
        .await
        .expect("enqueue active input");
    assert_eq!(
        pending_status(&store, &session_id, &input.input_id).await,
        (
            Some(crate::PendingTurnInputReadStatus::Open),
            Some(crate::PendingTurnInputReadStatus::Open)
        ),
        "an enqueued input reads open by id and in the list"
    );
    let fence = seal_drive_fence_for_test(&store, &session_id, "checkpoint-applied-owner").await;
    let admission = admit_at_checkpoint_for_test(
        &store,
        &fence,
        &turn,
        &turn,
        crate::CheckpointKind::AfterWork,
        "checkpoint-applied-binding:step",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("admit at the checkpoint");
    assert!(
        admission
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.input_ids() == vec![input.input_id.clone()]),
        "the checkpoint admits the input"
    );
    assert_eq!(
        store
            .root_of_input(&session_id, &input.input_id)
            .await
            .expect("read the in-flight binding"),
        None,
        "a checkpoint delivery in flight binds no root yet"
    );
    assert_eq!(
        pending_status(&store, &session_id, &input.input_id).await,
        (
            Some(crate::PendingTurnInputReadStatus::Admitted { root: turn.clone() }),
            Some(crate::PendingTurnInputReadStatus::Admitted { root: turn.clone() })
        ),
        "an accepted checkpoint delivery reads admitted to its root, by id and in the list, \
         until the root settles it"
    );

    end_root(
        &store,
        &fence,
        completing_checkpoint(IngressSettlement::new(turn.clone()), &admission),
    )
    .await;
    assert_eq!(
        store
            .root_of_input(&session_id, &input.input_id)
            .await
            .expect("read the applied binding"),
        Some(turn.clone()),
        "the completing commit binds the input to the root that applied it"
    );
    assert_eq!(
        pending_status(&store, &session_id, &input.input_id).await,
        (None, None),
        "the completing commit takes the input out of the pending read, by id and in the list"
    );
    assert_eq!(
        store
            .root_binding(&session_id, &input.input_id)
            .await
            .expect("read the applied binding"),
        Some(turn),
    );
}

/// `input`'s pending-read status by its point read and by the session's list.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the reads are established by the caller's setup"
)]
async fn pending_status(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    input: &crate::InputId,
) -> (
    Option<crate::PendingTurnInputReadStatus>,
    Option<crate::PendingTurnInputReadStatus>,
) {
    let by_id = store
        .pending_turn_input(session_id, input)
        .await
        .expect("read the row by id")
        .map(|read| read.status);
    let listed = store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list the pending rows")
        .into_iter()
        .find(|read| read.input.input_id == *input)
        .map(|read| read.status);
    (by_id, listed)
}

/// FIG-3927 N3, checkpoint half: `admit_at_checkpoint` is idempotent by
/// `(root, step)`. Run again under the same fence, under a later fence, or
/// with rows enqueued in between, the step reads back exactly the rows it
/// bound, byte for byte, and takes nothing more.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_admission_is_idempotent_by_root_and_step(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("checkpoint-step-idempotence");
    let turn_id = crate::TurnId::from("checkpoint-step-idempotence:turn");
    let step = "checkpoint-step-idempotence:step";
    store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "first checkpoint input",
        ))
        .await
        .expect("enqueue checkpoint input");
    store
        .enqueue_queued_work(queued_draft(
            &session_id,
            "first checkpoint wake",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let first = seal_drive_fence_for_test(&store, &session_id, "checkpoint-step-a").await;
    let admitted = at_checkpoint(
        &store,
        &first,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        step,
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("the first execution admits");
    assert!(!admitted.is_empty());
    let recorded = serde_json::to_value(&admitted).expect("encode the admission");

    let same_fence = at_checkpoint(
        &store,
        &first,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        step,
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("a rerun under the same fence");
    assert_eq!(
        serde_json::to_value(&same_fence).expect("encode the rerun"),
        recorded,
        "a rerun under the same fence reads its own rows back"
    );

    store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "later checkpoint input",
        ))
        .await
        .expect("enqueue a later input");
    store
        .enqueue_queued_work(queued_draft(
            &session_id,
            "later checkpoint wake",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue later queued work");
    store
        .supersede_drive_epoch_for_test(&first)
        .await
        .expect("the first drive is superseded");
    let successor = seal_drive_fence_for_test(&store, &session_id, "checkpoint-step-b").await;
    let resumed = at_checkpoint(
        &store,
        &successor,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        step,
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("a rerun under a later fence");
    assert_eq!(
        serde_json::to_value(&resumed).expect("encode the resumed rerun"),
        recorded,
        "a rerun under a later fence reads back exactly the recorded rows, not the later ones"
    );
    assert!(
        at_checkpoint(
            &store,
            &first,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            step,
            10,
            crate::testing::queued_work_admission_policy(10),
        )
        .await
        .is_err_and(|error| matches!(error, StoreError::StaleDriveFence { .. })),
        "the superseded fence reads nothing back"
    );
}

/// FIG-3927 N4 at a checkpoint (FIG-4044): a stale fence is refused
/// `StaleDriveFence` whatever the request's caps and whatever the checkpoint
/// has pending, and the step's read-back does not depend on the caps either.
/// A checkpoint whose caps are zero is still a checkpoint of a root that may
/// have been superseded, and a re-execution of its step still reads back the
/// rows the step bound.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_checkpoint_refuses_a_stale_fence_whatever_its_caps(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("checkpoint-stale-fence-caps");
    let turn_id = crate::TurnId::from("checkpoint-stale-fence-caps:turn");
    let idle_turn = crate::TurnId::from("checkpoint-stale-fence-caps:idle-turn");
    let step = "checkpoint-stale-fence-caps:step";
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "input a stale checkpoint must not take",
        ))
        .await
        .expect("enqueue checkpoint input");
    let stale = seal_drive_fence_for_test(&store, &session_id, "checkpoint-stale-caps-a").await;
    let live = seal_drive_fence_for_test(&store, &session_id, "checkpoint-stale-caps-b").await;
    assert!(live.epoch() > stale.epoch(), "the later seal supersedes");

    for turn in [&turn_id, &idle_turn] {
        for (max_inputs, max_rows) in [(10, 10), (0, 0), (0, 10), (10, 0)] {
            let refused = at_checkpoint(
                &store,
                &stale,
                turn,
                crate::CheckpointKind::AfterWork,
                step,
                max_inputs,
                crate::testing::queued_work_admission_policy(max_rows),
            )
            .await;
            assert!(
                matches!(refused, Err(StoreError::StaleDriveFence { .. })),
                "turn `{turn}`, caps ({max_inputs}, {max_rows}): a stale fence is refused, got \
                 {refused:?}"
            );
        }
    }

    let admitted = at_checkpoint(
        &store,
        &live,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        step,
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("the live fence admits");
    assert_eq!(
        admitted_input_ids(&admitted),
        vec![input.input_id.to_string()],
        "no stale attempt took the input"
    );
    let reread = at_checkpoint(
        &store,
        &live,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        step,
        0,
        crate::testing::queued_work_admission_policy(0),
    )
    .await
    .expect("a re-execution with zero caps");
    assert_eq!(
        serde_json::to_value(&reread).expect("encode the re-execution"),
        serde_json::to_value(&admitted).expect("encode the admission"),
        "a re-execution reads back the step's rows whatever its caps"
    );
}

/// FIG-4044: an input a checkpoint admitted is `accepted` into the running
/// turn and bound to its root, so the pending read reports it
/// `Admitted { root }` in its `enqueue_seq` place, exactly as it reports a row
/// the root's own admission took, until the root settles or releases it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_checkpoint_admitted_input_is_listed_admitted_to_its_root(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("checkpoint-admitted-listing");
    let turn_id = crate::TurnId::from("checkpoint-admitted-listing:turn");
    let before = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session_id,
            "next-turn input before",
        ))
        .await
        .expect("enqueue the earlier next-turn input");
    let accepted = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint input",
        ))
        .await
        .expect("enqueue the checkpoint input");
    let after = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session_id,
            "next-turn input after",
        ))
        .await
        .expect("enqueue the later next-turn input");
    let fence = seal_drive_fence_for_test(&store, &session_id, "checkpoint-admitted-listing").await;
    let admitted = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "checkpoint-admitted-listing:step",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("the checkpoint admits its input");
    assert_eq!(
        admitted_input_ids(&admitted),
        vec![accepted.input_id.to_string()]
    );

    let listed = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list pending inputs")
        .into_iter()
        .map(|read| (read.input.input_id, read.input.state.kind(), read.status))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![
            (
                before.input_id,
                crate::TurnInputStateKind::DeferredNextTurn,
                crate::PendingTurnInputReadStatus::Open,
            ),
            (
                accepted.input_id,
                crate::TurnInputStateKind::Accepted,
                crate::PendingTurnInputReadStatus::Admitted { root: turn_id },
            ),
            (
                after.input_id,
                crate::TurnInputStateKind::DeferredNextTurn,
                crate::PendingTurnInputReadStatus::Open,
            ),
        ],
        "the accepted input is listed admitted to its root, in enqueue order"
    );
}

/// `TurnInputIngress::ActiveTurn { min_boundary }` must be honored at every
/// checkpoint a backend can be asked about: `BeforeCompletion` ingress is
/// withheld at `AfterWork` and admitted at `BeforeCompletion`; `AfterWork`
/// ingress is admitted at both (FIG-1524).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_claims_honor_min_boundary_at_every_checkpoint(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("checkpoint-min-boundary");
    let turn_id = crate::TurnId::from("checkpoint-min-boundary:turn");
    let before_completion = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "withheld until before-completion",
        ))
        .await
        .expect("enqueue before-completion input");
    let fence =
        seal_drive_fence_for_test(&store, &session_id, "checkpoint-min-boundary-owner").await;

    let probed = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "min-boundary:step:1",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("probe after-work checkpoint holding only before-completion ingress");
    assert!(
        probed.is_empty(),
        "before-completion ingress must not be admitted at the after-work checkpoint"
    );

    let mut after_work = Vec::new();
    for text in [
        "admitted at the first after-work checkpoint",
        "admitted at the second after-work checkpoint",
        "still pending at before-completion",
    ] {
        after_work.push(
            store
                .enqueue_pending_turn_input(pending_active_turn_input_draft(
                    &session_id,
                    &turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                    text,
                ))
                .await
                .expect("enqueue after-work input"),
        );
    }

    for (step, expected) in [
        ("min-boundary:step:2", &after_work[0]),
        ("min-boundary:step:3", &after_work[1]),
    ] {
        let admitted = at_checkpoint(
            &store,
            &fence,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            step,
            1,
            crate::testing::queued_work_admission_policy(10),
        )
        .await
        .expect("admit after-work checkpoint work");
        assert!(admitted.queued.is_none(), "no queued work was enqueued");
        assert_eq!(
            admitted_input_ids(&admitted),
            vec![expected.input_id.to_string()],
            "the after-work checkpoint must admit after-work ingress, skipping the earlier \
             before-completion row rather than stalling on it"
        );
    }

    let admitted = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::BeforeCompletion,
        "min-boundary:step:4",
        10,
        crate::testing::queued_work_admission_policy(10),
    )
    .await
    .expect("admit before-completion checkpoint work");
    assert!(admitted.queued.is_none(), "no queued work was enqueued");
    assert_eq!(
        admitted_input_ids(&admitted),
        vec![
            before_completion.input_id.to_string(),
            after_work[2].input_id.to_string(),
        ],
        "the before-completion checkpoint must admit both boundaries in enqueue order"
    );
}

/// A checkpoint admission spans pending inputs and queued work atomically.
/// If the queued head cannot fit the context window, the active-turn input
/// must remain pending and open rather than bound to a refused admission.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_budget_refusal_preserves_active_turn_input(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("checkpoint-budget-atomicity");
    let turn_id = crate::TurnId::from("checkpoint-budget-atomicity:turn");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "input that must survive a queue budget refusal",
        ))
        .await
        .expect("enqueue active-turn input for atomic checkpoint admission");
    let oversized_text = "oversized queued work".repeat(64);
    store
        .enqueue_queued_work(queued_draft(
            &session_id,
            &oversized_text,
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue oversized checkpoint queued work");
    let fence =
        seal_drive_fence_for_test(&store, &session_id, "checkpoint-budget-atomicity-owner").await;
    let error = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "checkpoint-budget:step",
        10,
        crate::TurnLaneAdmissionPolicy {
            max_context_tokens: 64,
            action_token_reserve: 1,
            max_rows: 10,
            max_pending_age_ms: 30_000,
            drain_policy: crate::default_queued_drain_policy(),
        },
    )
    .await
    .expect_err("oversized queued row must refuse the combined checkpoint admission");
    assert!(matches!(
        error,
        StoreError::QueuedWorkRowExceedsContextWindow { .. }
    ));

    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("list active input after checkpoint budget refusal");
    assert_eq!(
        pending
            .iter()
            .map(|read| read.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "the input binding must roll back with the refused queued-work admission"
    );
    assert_eq!(
        pending[0].input.state.kind(),
        crate::TurnInputStateKind::PendingActive
    );
    assert_eq!(pending[0].status, crate::PendingTurnInputReadStatus::Open);
}

/// Prove checkpoint admission probes stay read-only for empty queues and for
/// deferred queue heads, while real checkpoint work still shares one write
/// transaction and deferred work remains admissible at the idle boundary.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn checkpoint_admission_probe_transaction_counts(
    store: Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    counts: impl Fn() -> (usize, usize),
) {
    let turn_id = crate::TurnId::from(format!("{session_id}:counter-turn"));
    let fence = seal_drive_fence_for_test(
        &store,
        session_id,
        &format!("{session_id}:checkpoint-counter-owner"),
    )
    .await;

    let empty = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "counter:step:1",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("probe quiescent checkpoint");
    assert!(empty.is_empty());
    assert_eq!(counts(), (1, 0));

    let deferred = store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "deferred checkpoint head",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue deferred checkpoint head");
    let deferred_checkpoint = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "counter:step:2",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("probe deferred checkpoint head");
    assert!(
        deferred_checkpoint.is_empty(),
        "after-current-turn-commit work must not be admitted at an active checkpoint"
    );
    assert_eq!(
        counts(),
        (2, 0),
        "a deferred queue head must not open a checkpoint write transaction"
    );

    let idle = admitted_root(
        &store,
        &fence,
        "counter-idle-root",
        lash_core::store::AdmittedHead::Batch(deferred.batch_id.clone()),
    )
    .await;
    assert_eq!(idle.batch_ids(), vec![deferred.batch_id.clone()]);

    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session_id,
            crate::TurnInputIngress::active_turn(
                turn_id.to_string(),
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("pending checkpoint input"),
        ))
        .await
        .expect("enqueue counter input");
    store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "pending checkpoint work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue counter work");
    let pending = at_checkpoint(
        &store,
        &fence,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "counter:step:3",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("admit pending checkpoint work");
    assert!(pending.inputs.is_some() && pending.queued.is_some());
    assert_eq!(counts(), (3, 1));
}

pub fn queued_process_wake_draft(
    session_id: &SessionId,
    text: &str,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    let wake = ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("wake:{session_id}:{text}"),
        target_session_id: session_id.clone(),
        process_id: crate::ProcessId::fixture(&format!("process:{text}")),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: RuntimeAttribution::for_session(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: crate::ProcessId::fixture(&format!("process:{text}")),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    };
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        crate::TurnWorkPayload::process_wake(wake),
    )
    .with_source_key(crate::process_wake_source_key(
        &crate::ProcessId::fixture(&format!("process:{text}")),
        1,
    ))
    .with_process_wake_source(crate::ProcessId::fixture(&format!("process:{text}")), 1)
}

/// Some queued turn work carrying `text`: a process wake, the one turn-work
/// payload (a frame handoff is a head fact, never a queue row; ADR 0101 §3).
pub(super) fn queued_draft(
    session_id: &SessionId,
    text: &str,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    queued_process_wake_draft(session_id, text, delivery_policy)
}

/// Queued turn work carrying `text` whose source is `key`: the wake of
/// process `key` at sequence 1, so the source key is that wake's own and the
/// same `key` names the same row.
pub(super) fn keyed_queued_draft(
    session_id: &SessionId,
    text: &str,
    delivery_policy: DeliveryPolicy,
    key: impl AsRef<str>,
) -> QueuedWorkBatchDraft {
    crate::conformance::helpers::process_wake_work(
        session_id,
        key.as_ref(),
        1,
        text,
        delivery_policy,
    )
}

pub(super) fn queued_session_command_draft(
    session_id: &SessionId,
    reason: &str,
) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        DeliveryPolicy::EarliestSafeBoundary,
        crate::SessionCommand::RefreshToolCatalog {
            reason: reason.to_string(),
        },
    )
}

pub(super) fn queued_batch_text(batch: &QueuedWorkBatch) -> Option<&str> {
    let payload = batch.items.first().map(|item| &item.payload)?;
    match payload {
        QueuedWorkPayload::ProcessWake { wake } => Some(wake.input.as_str()),
        QueuedWorkPayload::SessionCommand { .. } => None,
    }
}

pub(super) fn pending_next_turn_input_draft(
    session_id: &SessionId,
    text: &str,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::NextTurn,
        crate::TurnInput::text(text),
    )
}

#[expect(
    clippy::unwrap_used,
    reason = "conformance-law fixture: the unwrap mirrors the setup above"
)]
pub(super) fn inline_png(bytes: Vec<u8>) -> crate::AttachmentSource {
    crate::AttachmentSource::inline(crate::MediaType::parse("image/png").unwrap(), bytes)
}

pub(super) fn pending_active_turn_input_draft(
    session_id: &SessionId,
    turn_id: &TurnId,
    min_boundary: crate::TurnInputCheckpointBoundary,
    text: &str,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::active_turn(turn_id, min_boundary),
        crate::TurnInput::text(text),
    )
}

pub(super) fn pending_input_text(input: &crate::PendingTurnInput) -> Option<&str> {
    match input.input.items.first()? {
        crate::InputItem::Text { text } => Some(text.as_str()),
        crate::InputItem::Attachment { .. } => None,
    }
}

pub(super) fn expect_cancelled_pending_input(
    outcome: crate::PendingTurnInputCancelOutcome,
    input_id: &str,
) -> crate::PendingTurnInput {
    match outcome {
        crate::PendingTurnInputCancelOutcome::Cancelled(input) => {
            assert_eq!(input.input_id, input_id);
            assert_eq!(input.state.kind(), crate::TurnInputStateKind::Cancelled);
            input
        }
        other => panic!("expected cancelled pending turn input `{input_id}`, got {other:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) fn sample_session_node(
    session_id: &SessionId,
    id: &str,
    parent: Option<&str>,
) -> SessionNodeRecord {
    let frame_key = crate::FrameKey::from_caller_material(id).expect("non-empty frame material");
    let node_id = parent.map_or_else(
        || caller_frame_node_id(session_id, id).into_inner(),
        |_| id.to_string(),
    );
    SessionNodeRecord {
        node_id: node_id.into(),
        parent_node_id: parent.map(lash_core::NodeId::from),
        timestamp: "1970-01-01T00:00:00Z".to_string(),
        payload: if parent.is_none() {
            SessionNodePayload::FrameOpen {
                frame_key,
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
                protocol_turn_options: ProtocolTurnOptions::default(),
            }
        } else {
            SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    ProtocolEvent::typed("conformance", serde_json::json!({ "node": id }))
                        .expect("protocol event"),
                ),
            }
        },
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) fn caller_frame_node_id(session_id: &SessionId, material: &str) -> crate::FrameNodeId {
    let frame_key =
        crate::FrameKey::from_caller_material(material).expect("non-empty frame material");
    crate::frame_node_id(session_id, frame_key.as_str())
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) fn attachment_intent(id: &str) -> AttachmentWrite {
    crate::AttachmentWrite {
        attachment_id: AttachmentId::parse(id).expect("valid attachment id"),
        claim: crate::conformance::attachment_referrers::claim(crate::ArtifactReferrer::Session(
            SessionId::from("root"),
        )),
    }
}
