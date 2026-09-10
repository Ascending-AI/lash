use super::*;
use pretty_assertions::assert_eq;

/// A backend must mint refs for checkpoint bodies and resolve those refs after
/// both the ref-only successor write and the final read reopen the substrate.
///
/// This is the standing regression for the checkpoint-component failure
/// shape: write bodies, drop the writer, write only their refs through an
/// independently constructed handle, drop that writer, then construct a third
/// handle and hydrate. The helper owns construction order so a caller cannot
/// prebuild nominally cold handles before the writes they verify.
pub async fn complete_runtime_checkpoint_component_set_survives_cold_reopens<F>(make: F)
where
    F: Fn() -> Arc<dyn RuntimePersistence>,
{
    let open = make();
    let open_identity = Arc::downgrade(&open);
    bind_conformance_session(&open, "checkpoint-component-refs").await;
    let mut state = RuntimeSessionState {
        session_id: "checkpoint-component-refs".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_execution_state_snapshot(Some(b"known-execution-state".to_vec()));
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
    bind_conformance_session(&reopen, "checkpoint-component-refs").await;

    // Exercise the production hydration and ordinary commit boundary. No test
    // code re-inserts arbitrary keys: the runtime-owned complete set must carry
    // them as unchanged refs.
    state = crate::store::load_persisted_session_state(reopen.as_ref())
        .await
        .expect("hydrate resident checkpoint component set")
        .expect("seeded checkpoint state");
    let mut ordinary_turn_projection = state.to_snapshot();
    ordinary_turn_projection.turn_index += 1;
    state.apply_snapshot(&ordinary_turn_projection);
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
            .manifest()
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
    state = crate::store::load_persisted_session_state(reopen.as_ref())
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
    bind_conformance_session(&cold_reopen, "checkpoint-component-refs").await;
    let read = cold_reopen
        .load_session()
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

    state = crate::store::load_persisted_session_state(cold_reopen.as_ref())
        .await
        .expect("reload current state before rejection laws")
        .expect("current checkpoint state before rejection laws");
    let mut unknown = RuntimeCommit::persisted_state_for_test(&state, &[]);
    unknown.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("never-stored".to_string()),
                encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
            },
        },
    );
    let rejection_lease = claim_session_execution_lease_for_test(
        &cold_reopen,
        "checkpoint-component-refs",
        "checkpoint-component-rejections",
    )
    .await;
    let unknown_error = cold_reopen
        .commit_runtime_state(
            unknown.releasing_session_execution_lease(rejection_lease.completion()),
        )
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
            encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION + 1,
            body_ref: crate::store::BlobRef::for_content(b"unsupported"),
            body: b"unsupported".to_vec(),
        },
    );
    let mismatch_error = cold_reopen
        .commit_runtime_state(
            mismatch.releasing_session_execution_lease(rejection_lease.completion()),
        )
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
    release_session_execution_lease_for_test(&cold_reopen, &rejection_lease).await;
}

/// A ref-only checkpoint commit is valid only when every referenced component
/// already exists in the backend.
pub async fn checkpoint_rejects_unknown_component_ref(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: "checkpoint-unknown-ref".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        "arbitrary/unknown-ref".to_string(),
        crate::HydratedCheckpointComponent::Unchanged {
            descriptor: crate::CheckpointComponentDescriptor {
                blob_ref: crate::BlobRef("checkpoint-component-that-was-never-stored".to_string()),
                encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
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

pub(super) async fn commit_rejects_leaf_without_frame_open_ancestor(
    store: Arc<dyn RuntimePersistence>,
) {
    let state = RuntimeSessionState {
        session_id: "missing-frame-root".to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let node = SessionNodeRecord {
        node_id: "unframed-root".to_string(),
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
        crate::GraphAppend {
            nodes: vec![node],
            leaf_node_id: Some("unframed-root".to_string()),
        },
        &[],
    );
    let expected_leaf_node_id = commit
        .graph
        .leaf_node_id
        .clone()
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

pub(super) async fn turn_input_application_identity_survives_pending_tombstone_vacuum(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "turn-input-application";
    let owner_id = "turn-input-application-owner";
    let lease = claim_session_execution_lease_for_test(&store, session_id, owner_id).await;
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut expected = Vec::new();
    let mut replay = None;

    for (turn_index, turn_id) in ["z-first-application-turn", "a-second-application-turn"]
        .into_iter()
        .enumerate()
    {
        let admitted = futures_util::future::try_join_all((0..2).map(|input_index| {
            store.enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    session_id,
                    &format!("canonical application {turn_index}:{input_index}"),
                )
                .with_source_key(format!(
                    "host:application-source-{turn_index}-{input_index}"
                )),
            )
        }))
        .await
        .expect("enqueue application inputs");
        let mut claim = store
            .claim_next_turn_inputs(session_id, &lease.fence(), &lease_owner(owner_id), 10)
            .await
            .expect("claim application inputs")
            .expect("application input claim");
        let committed_message_id = format!("application-message-{turn_index}");
        claim.record_initial_turn_application(&crate::TurnId::from(turn_id), &committed_message_id);
        let turn_expected = claim.applications.clone();
        assert_eq!(admitted.len(), turn_expected.len());

        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(lease.completion());
        }
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
            .list_turn_input_applications(session_id)
            .await
            .expect("read durable application identity"),
        expected,
        "applications must follow monotonic turn-commit order and must not double-count a replay"
    );

    store.vacuum().await.expect("vacuum application tombstone");
    assert_eq!(
        store
            .list_turn_input_applications(session_id)
            .await
            .expect("read application identity after tombstone vacuum"),
        expected,
        "application reconciliation must come from the committed turn, not a pending snapshot"
    );
}

pub(super) async fn checkpoint_work_claims_both_families_once(store: Arc<dyn RuntimePersistence>) {
    let session_id = "checkpoint-work";
    let turn_id = crate::TurnId::from("checkpoint-turn");
    let owner = lease_owner("checkpoint-owner");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "checkpoint input",
        ))
        .await
        .expect("enqueue checkpoint input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "checkpoint queued work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-work-claims-both-families-once-executor",
            60_000,
        )
        .await
        .expect("claim checkpoint session lease")
        .acquired()
        .expect("checkpoint session lease acquired");

    let (input_claim, queue_claim) = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim both checkpoint work families");
    let input_claim = input_claim.expect("checkpoint input claim exists");
    let queue_claim = queue_claim.expect("checkpoint queue claim exists");
    assert_eq!(input_claim.inputs[0].input_id, input.input_id);
    assert_eq!(queue_claim.batches[0].batch_id, batch.batch_id);
    assert_eq!(input_claim.session_lease_generation, lease.fencing_token);
    assert_eq!(queue_claim.session_lease_generation, lease.fencing_token);

    let second = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("same-generation checkpoint re-claim");
    assert!(
        second.0.is_none() && second.1.is_none(),
        "checkpoint claims must be granted exactly once per lease generation"
    );
}

/// `TurnInputIngress::ActiveTurn { min_boundary }` must be honored at every
/// checkpoint a backend can be asked about. This pins all four
/// boundary/checkpoint cells on both the admission-probe path
/// (`claim_checkpoint_work`) and the direct claim path
/// (`claim_active_turn_inputs`): `BeforeCompletion` ingress is withheld at
/// `AfterWork` and admitted at `BeforeCompletion`; `AfterWork` ingress is
/// admitted at both (FIG-1524).
pub(super) async fn checkpoint_claims_honor_min_boundary_at_every_checkpoint(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "checkpoint-min-boundary";
    let turn_id = crate::TurnId::from("checkpoint-min-boundary:turn");
    let owner = lease_owner("checkpoint-min-boundary-owner");
    let before_completion = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::BeforeCompletion,
            "withheld until before-completion",
        ))
        .await
        .expect("enqueue before-completion input");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-min-boundary-executor",
            60_000,
        )
        .await
        .expect("claim min-boundary session lease")
        .acquired()
        .expect("min-boundary session lease acquired");

    let probed = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("probe after-work checkpoint holding only before-completion ingress");
    assert!(
        probed.0.is_none() && probed.1.is_none(),
        "before-completion ingress must not be admitted at the after-work checkpoint"
    );
    assert!(
        store
            .claim_active_turn_inputs(
                session_id,
                &lease.fence(),
                &owner,
                &turn_id,
                crate::CheckpointKind::AfterWork,
                10,
            )
            .await
            .expect("direct after-work claim holding only before-completion ingress")
            .is_none(),
        "the direct claim path must honor min_boundary at the after-work checkpoint too"
    );

    let after_work_first = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "admitted at after-work",
        ))
        .await
        .expect("enqueue first after-work input");
    let after_work_second = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "admitted through the direct claim path",
        ))
        .await
        .expect("enqueue second after-work input");
    let after_work_third = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "still pending at before-completion",
        ))
        .await
        .expect("enqueue third after-work input");

    let (probe_claim, probe_queue) = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            1,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim after-work checkpoint work");
    assert!(probe_queue.is_none(), "no queued work was enqueued");
    assert_eq!(
        probe_claim
            .expect("after-work checkpoint input claim")
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![after_work_first.input_id.as_str()],
        "the after-work checkpoint must admit after-work ingress, skipping the earlier \
         before-completion row rather than stalling on it"
    );

    let after_work_claim = store
        .claim_active_turn_inputs(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            1,
        )
        .await
        .expect("claim after-work admitted input")
        .expect("after-work input claim");
    assert_eq!(
        after_work_claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![after_work_second.input_id.as_str()],
        "the direct claim path must admit after-work ingress at the after-work checkpoint"
    );

    let (input_claim, queue_claim) = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::BeforeCompletion,
            10,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim before-completion checkpoint work");
    assert!(queue_claim.is_none(), "no queued work was enqueued");
    let input_claim = input_claim.expect("before-completion input claim");
    assert_eq!(
        input_claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            before_completion.input_id.as_str(),
            after_work_third.input_id.as_str(),
        ],
        "the before-completion checkpoint must admit both boundaries in enqueue order"
    );
}

/// A checkpoint claim spans pending inputs and queued work atomically. If the
/// queued head cannot fit the context window, the active-turn input must remain
/// pending and visible rather than being left accepted under a discarded claim.
pub(super) async fn checkpoint_budget_refusal_preserves_active_turn_input(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "checkpoint-budget-atomicity";
    let turn_id = crate::TurnId::from("checkpoint-budget-atomicity:turn");
    let owner = lease_owner("checkpoint-budget-atomicity-owner");
    let input = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            session_id,
            &TurnId::from(turn_id.as_str()),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "input that must survive a queue budget refusal",
        ))
        .await
        .expect("enqueue active-turn input for atomic checkpoint claim");
    let oversized_text = "oversized queued work".repeat(64);
    store
        .enqueue_queued_work(queued_draft(
            session_id,
            &oversized_text,
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue oversized checkpoint queued work");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-budget-refusal-executor",
            60_000,
        )
        .await
        .expect("claim checkpoint atomicity session lease")
        .acquired()
        .expect("checkpoint atomicity session lease acquired");
    let error = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            10,
            crate::QueuedWorkClaimPolicy {
                max_context_tokens: 64,
                action_token_reserve: 1,
                max_rows: 10,
                max_pending_age_ms: 30_000,
                drain_policy: crate::default_queued_drain_policy(),
            },
        )
        .await
        .expect_err("oversized queued row must refuse the combined checkpoint claim");
    assert!(matches!(
        error,
        StoreError::QueuedWorkRowExceedsContextWindow { .. }
    ));

    let pending = store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list active input after checkpoint budget refusal");
    assert_eq!(
        pending
            .iter()
            .map(|row| row.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "the input claim must roll back with the refused queued-work claim"
    );
    assert_eq!(pending[0].state, crate::TurnInputState::PendingActive);
}

/// Prove checkpoint admission probes stay read-only for empty queues and for
/// deferred queue heads, while real checkpoint work still shares one write
/// transaction and deferred work remains claimable at the idle boundary.
pub async fn checkpoint_claim_probe_transaction_counts(
    store: Arc<dyn RuntimePersistence>,
    session_id: &str,
    counts: impl Fn() -> (usize, usize),
) {
    let turn_id = crate::TurnId::from(format!("{session_id}:counter-turn"));
    let owner = lease_owner(&format!("{session_id}:checkpoint-counter-owner"));
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            &owner,
            "checkpoint-claim-probe-transaction-counts-executor",
            60_000,
        )
        .await
        .expect("claim checkpoint counter lease")
        .acquired()
        .expect("checkpoint counter lease acquired");

    let empty = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("probe quiescent checkpoint");
    assert!(empty.0.is_none() && empty.1.is_none());
    assert_eq!(counts(), (1, 0));

    let deferred = store
        .enqueue_queued_work(queued_process_wake_draft(
            session_id,
            "deferred checkpoint head",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue deferred checkpoint head");
    let deferred_checkpoint = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("probe deferred checkpoint head");
    assert!(
        deferred_checkpoint.0.is_none() && deferred_checkpoint.1.is_none(),
        "after-current-turn-commit work must not claim at an active checkpoint"
    );
    assert_eq!(
        counts(),
        (2, 0),
        "a deferred queue head must not open a checkpoint write transaction"
    );

    let deferred_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim deferred work at idle boundary")
        .claim()
        .expect("deferred work remains claimable at idle boundary");
    assert_eq!(deferred_claim.batches[0].batch_id, deferred.batch_id);

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
    let pending = store
        .claim_checkpoint_work(
            session_id,
            &lease.fence(),
            &owner,
            &turn_id,
            crate::CheckpointKind::AfterWork,
            64,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim pending checkpoint work");
    assert!(pending.0.is_some() && pending.1.is_some());
    assert_eq!(counts(), (3, 1));
}

/// Build a queued process-wake draft for backend conformance tests.
pub fn queued_process_wake_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    let wake = ProcessWakeDelivery {
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("wake:{session_id}:{text}"),
        target_session_id: session_id.to_string(),
        process_id: format!("process:{text}"),
        process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: format!("process:{text}"),
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
        &format!("process:{text}"),
        1,
    ))
    .with_process_wake_source(format!("process:{text}"), 1)
}

pub(super) fn queued_draft(
    session_id: &str,
    text: &str,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        crate::TurnWorkPayload::agent_frame_task(
            crate::session_graph::frame_node_id(session_id, &format!("frame:{text}")),
            text,
            None,
        ),
    )
}

pub(super) fn queued_session_command_draft(session_id: &str, reason: &str) -> QueuedWorkBatchDraft {
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
        QueuedWorkPayload::AgentFrameTask { task, .. } => Some(task.as_str()),
        QueuedWorkPayload::SessionCommand { .. } => None,
    }
}

pub(super) fn pending_next_turn_input_draft(
    session_id: &str,
    text: &str,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        session_id,
        crate::TurnInputIngress::NextTurn,
        crate::TurnInput::text(text),
    )
}

pub(super) fn inline_png(bytes: Vec<u8>) -> crate::AttachmentSource {
    crate::AttachmentSource::inline(crate::MediaType::parse("image/png").unwrap(), bytes)
}

pub(super) fn pending_active_turn_input_draft(
    session_id: &str,
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
            assert_eq!(input.state, crate::TurnInputState::Cancelled);
            input
        }
        other => panic!("expected cancelled pending turn input `{input_id}`, got {other:?}"),
    }
}

pub(super) fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
    crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

pub(super) async fn release_session_execution_lease_for_test(
    store: &Arc<dyn RuntimePersistence>,
    lease: &crate::SessionExecutionLease,
) {
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .expect("release session execution lease");
}

pub(super) fn sample_session_node(
    session_id: &str,
    id: &str,
    parent: Option<&str>,
) -> SessionNodeRecord {
    let frame_key = crate::FrameKey::from_caller_material(id).expect("non-empty frame material");
    let node_id = parent.map_or_else(
        || caller_frame_node_id(session_id, id).into_inner(),
        |_| id.to_string(),
    );
    SessionNodeRecord {
        node_id,
        parent_node_id: parent.map(ToOwned::to_owned),
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

pub(super) fn caller_frame_node_id(session_id: &str, material: &str) -> crate::FrameNodeId {
    let frame_key =
        crate::FrameKey::from_caller_material(material).expect("non-empty frame material");
    crate::frame_node_id(session_id, frame_key.as_str())
}

pub(super) fn attachment_intent(id: &str) -> AttachmentIntent {
    AttachmentIntent {
        attachment_id: AttachmentId::parse(id).expect("valid attachment id"),
        session_id: "root".to_string(),
        canonical_uri: format!("sha256:{id}"),
        intent_at_epoch_ms: 100,
        owner_kind: None,
        owner_id: None,
    }
}
