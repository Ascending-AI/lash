use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION;
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.set_execution_state_snapshot(Some(b"known-execution-state".to_vec().into()));
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&state);
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
    let second_commit = RuntimeCommit::persisted_state_for_test(&state);
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
    let mut third_commit = RuntimeCommit::persisted_state_for_test(&state);
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
    let mut unknown = RuntimeCommit::persisted_state_for_test(&state);
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
    let unknown_error = cold_reopen
        .commit_runtime_state(unknown)
        .await
        .expect_err("arbitrary unknown ref must fail");
    assert!(matches!(
        unknown_error,
        StoreError::CheckpointComponentMissing { ref key, .. }
            if key == "arbitrary/unknown-ref"
    ));

    let mut mismatch = RuntimeCommit::persisted_state_for_test(&state);
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state);
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
        session_id: SessionId::from("missing-frame-run"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let node = SessionNodeRecord {
        node_id: "unframed-root".into(),
        parent_node_id: None,
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Event {
            event: crate::SessionHistoryRecord::Protocol(
                ProtocolEvent::typed("unframed", serde_json::Value::Null).expect("protocol event"),
            ),
        },
    };
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend { nodes: vec![node] },
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

/// Admit what `turn`'s checkpoint `kind` takes at `step`, `turn` being its
/// own run.
async fn at_checkpoint(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    turn: &TurnId,
    kind: crate::CheckpointKind,
    step: &str,
    max_inputs: usize,
    policy: crate::TurnLaneAdmissionPolicy,
) -> Result<lash_core::store::CheckpointAdmission, StoreError> {
    admit_at_checkpoint_for_test(
        store, session_id, turn, turn, kind, step, max_inputs, policy,
    )
    .await
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
    let turn_id = crate::TurnId::fixture(format!("{session_id}:counter-turn"));

    let empty = at_checkpoint(
        &store,
        session_id,
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
        session_id,
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

    // The run whose checkpoint the rest probes starts; the deferred head
    // leaves the lane.
    active_run(&store, session_id, &turn_id).await;
    store
        .cancel_queued_work_batch(session_id, &deferred.batch_id)
        .await
        .expect("withdraw the deferred head")
        .expect("the deferred head is open");

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
        session_id,
        &turn_id,
        crate::CheckpointKind::AfterWork,
        "counter:step:3",
        64,
        crate::testing::queued_work_admission_policy(64),
    )
    .await
    .expect("admit pending checkpoint work");
    assert!(pending.queued.is_some());
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
        target_session_id: session_id.clone(),
        process_id: crate::ProcessId::fixture(&format!("process:{text}")),
        sequence: 1,
        event_type: "process.wake".to_string(),
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
        trace_cause: Default::default(),
    };
    QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        crate::QueuedWorkPayload::process_wake(wake),
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
    match &batch.payload {
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
        node_id: lash_core::NodeId::fixture(node_id),
        parent_node_id: parent.map(lash_core::NodeId::fixture),
        timestamp: "1970-01-01T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: if parent.is_none() {
            SessionNodePayload::FrameOpen {
                frame_key,
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )),
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
