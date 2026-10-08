//! Cold-reopen, session metadata, blob GC and graph-commit conformance laws.
//!
//! Split out of `turn_inputs_and_reopen.rs` to keep that file under the line
//! budget; every law keeps its name and its registration path.

use super::*;
use pretty_assertions::assert_eq;

/// Metadata written through the store round-trips.
///
/// The fixture admitted this session as a run, and the recorded lineage is
/// write-once (FIG-3045), so this law rewrites exactly what the production
/// caller rewrites: the same relation with its pending observer intents
/// settled. Child and fork relations round-trip through
/// `session_store_factory_round_trips_every_relation_shape`, which declares the
/// lineage at admission on the same three backends.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_metadata_round_trips(store: Arc<dyn RuntimeStore>) {
    let meta = SessionMeta {
        owning_process_id: None,
        pending_observer_intents: vec![
            crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture("observer-a")),
            crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture("observer-b")),
        ],
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    store
        .settle_observer_intents(&meta.session_id, meta.pending_observer_intents.clone())
        .await
        .expect("save session meta");
    let loaded = store
        .load_session_meta(&SessionId::from("root"))
        .await
        .expect("load session meta")
        .expect("session meta present");
    assert_eq!(loaded, meta);
}

/// Settling observers preserves the relation, creation provenance and process owner.
#[expect(clippy::expect_used, reason = "conformance fixture")]
pub async fn observer_settlement_preserves_creation_facts(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("observer-creation-facts");
    let relation = SessionRelation::Child {
        parent_session_id: SessionId::from("root"),
        caused_by: Some(crate::CausalRef::Turn {
            session_id: SessionId::from("root"),
            turn_id: TurnId::from("creation"),
        }),
    };
    let mut request = lash_core::testing::store_fixtures::session_store_request(
        &session_id,
        "conformance-model",
        relation.clone(),
    );
    request.owning_process_id = Some(crate::ProcessId::fixture("owner"));
    request.pending_observer_intents = vec![crate::SessionObserverIntent::host_requested(
        crate::ProcessId::fixture("observer"),
    )];
    store
        .admit_session(&request)
        .await
        .expect("admit creation facts");
    let recorded = store
        .load_session_meta(&session_id)
        .await
        .expect("load creation")
        .expect("admitted row");
    let remaining = vec![crate::SessionObserverIntent::host_requested(
        crate::ProcessId::fixture("remaining-observer"),
    )];
    store
        .settle_observer_intents(&session_id, remaining.clone())
        .await
        .expect("retain unresolved observer");
    let mut expected_pending = recorded.clone();
    expected_pending.pending_observer_intents = remaining.clone();
    assert_eq!(
        store
            .load_session_meta(&session_id)
            .await
            .expect("read pending")
            .expect("row retained"),
        expected_pending
    );
    store
        .settle_observer_intents(&session_id, remaining)
        .await
        .expect("replay settlement");
    store
        .settle_observer_intents(&session_id, Vec::new())
        .await
        .expect("clear observers");
    let mut expected = recorded;
    expected.pending_observer_intents.clear();
    assert_eq!(
        store
            .load_session_meta(&session_id)
            .await
            .expect("read settled")
            .expect("row retained"),
        expected
    );
}

/// Observer settlement must never create metadata without session admission.
#[expect(clippy::expect_used, reason = "conformance fixture")]
pub async fn observer_settlement_requires_admission(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("unadmitted-observer-session");
    let result = store.settle_observer_intents(&session_id, Vec::new()).await;
    assert!(
        matches!(result, Err(StoreError::SessionNotFound { session_id: ref missing }) if missing == session_id),
        "observer settlement must refuse missing admission, got {result:?}"
    );
    assert!(
        store
            .load_session_meta(&session_id)
            .await
            .expect("read missing session")
            .is_none()
    );
}

/// Blob-backed backends must physically reclaim the checkpoint blob a superseding
/// commit orphaned, while preserving the live one. Generalizes the SQLite-only
/// `gc_unreachable_keeps_rooted_checkpoint_blobs` test to every reclaiming
/// backend via the [`GcReport`](crate::GcReport) counters plus a post-GC load.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn gc_blobs(factory: ReopenableRuntimeStore) {
    let store = factory.open;
    // First commit writes a live checkpoint blob.
    let mut v1 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    v1.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(1),
    ));
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v1),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let mut v2 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        head_revision: v1_result.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    v2.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(2),
    ));
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v2),
        "gc-blobs-v2",
    )
    .await
    .expect("commit v2");

    let report = store
        .gc_unreachable()
        .await
        .expect("gc reclaims unreachable checkpoint blobs");
    assert!(
        report.root_count >= 1,
        "a live checkpoint must be rooted, got {report:?}"
    );
    assert!(
        report.retained_blob_count >= 1,
        "the live checkpoint blob must be retained, got {report:?}"
    );
    assert!(
        report.deleted_blob_count >= 1,
        "the superseded checkpoint blob must be reclaimed, got {report:?}"
    );

    // The reachable checkpoint survived: the session still loads at generation 2.
    let read = store
        .load_session_window(
            &SessionId::from("gc-blobs"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load after gc")
        .expect("session after gc");
    assert_eq!(
        read.checkpoint
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reachable tool state")
            })
            .map(|tool_state| tool_state.generation()),
        Some(2),
        "gc must preserve the reachable checkpoint's snapshots"
    );

    // Idempotent: with nothing newly unreachable, a second sweep deletes nothing.
    let second = store.gc_unreachable().await.expect("second gc");
    assert_eq!(
        second.deleted_blob_count, 0,
        "gc must never reclaim reachable blobs, got {second:?}"
    );
}

/// Manifest rows are GC roots, not read authorization (FIG-653).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn attachment_acquisition_preserves_receiving_referrer(store: Arc<dyn RuntimeStore>) {
    let id = AttachmentId::parse("acquire-reference").expect("id");
    let source = crate::ArtifactReferrer::ProcessRecord(crate::ProcessId::fixture("source"));
    crate::conformance::helpers::record_completed_attachment_write(
        &store,
        crate::AttachmentWrite {
            attachment_id: id.clone(),
            claim: crate::ReferrerClaim::unguarded(source.clone()).expect("claim"),
        },
    )
    .await;
    let receiver = crate::ArtifactReferrer::Session("root".into());
    store
        .acquire_attachment_refs(
            &crate::ReferrerClaim::unguarded(receiver.clone()).expect("receiver"),
            std::slice::from_ref(&id),
        )
        .await
        .expect("acquire");
    store
        .end_attachment_referrer(&source)
        .await
        .expect("end source");
    assert_eq!(
        store.attachment_referrers(&id).await.expect("refs"),
        vec![receiver]
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_receipt_reopen(factory: ReopenableRuntimeStore) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let nodes = vec![crate::SessionAppendNode::plugin(
        "append-receipt-reopen",
        serde_json::json!({"value": "reopen"}),
    )];
    let (first_commit, _) =
        append_request_commit(&mut state, "append-receipt-reopen", &nodes, None);
    let first =
        commit_runtime_state_for_test(&factory.open, first_commit, "append-receipt-reopen-first")
            .await
            .expect("commit append receipt before reopen");

    let mut reopened_state =
        loaded_conformance_state(&factory.reopen, &SessionId::from("root")).await;
    let (retry_commit, _) =
        append_request_commit(&mut reopened_state, "append-receipt-reopen", &nodes, None);
    let replay = factory
        .reopen
        .commit_runtime_state(retry_commit)
        .await
        .expect("reopened store replays append receipt");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, first.head_revision);
    assert_eq!(replay.checkpoint_ref, first.checkpoint_ref);
    assert_eq!(replay.committed_leaf_node_id, first.committed_leaf_node_id);
    assert_eq!(
        replay.realized_node_timestamps,
        first.realized_node_timestamps
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_non_derived_append_node_ids(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "guard-turn", "final");
    let graph = crate::GraphAppend::Extend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "rogue-node-id".into(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "guard".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"ok": true})),
            },
        }],
    };
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit(&state, graph);
    commit.turn_commit = RuntimeTurnCommitStamp::new(operation);
    let err = commit_runtime_state_for_test(&store, commit, "node-guard")
        .await
        .expect_err("store must rederive append node ids before writing");
    assert!(
        matches!(&err, StoreError::NodeIdDerivationMismatch { .. }),
        "unexpected node-derivation error: {err:?}"
    );
    assert!(
        store
            .load_session_window(
                &SessionId::from("root"),
                crate::store::WindowSelector::Current
            )
            .await
            .expect("load after guard rejection")
            .is_some_and(|window| window.head_revision == 0),
        "guard rejection must happen before any write past the created head"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_rejects_existing_node_id_collision(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let frame_key =
        crate::FrameKey::from_caller_material("collision-frame").expect("non-empty frame material");
    let colliding_id = crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    let original = crate::SessionNodeRecord {
        node_id: lash_core::NodeId::fixture(colliding_id.to_string()),
        parent_node_id: None,
        timestamp: "2026-07-26T10:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.clone(),
            reason: AgentFrameReason::new("original"),
            assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )),
        },
    };
    state.session_graph = crate::SessionGraph::from_nodes(
        vec![original.clone()],
        Some(lash_core::NodeId::fixture(colliding_id.to_string())),
    )
    .expect("collision fixture seed graph is valid");
    let initial = RuntimeCommit::persisted_state_for_test(&state);
    let first = commit_runtime_state_for_test(&store, initial, "collision-seed")
        .await
        .expect("seed colliding durable node");

    let replacement = crate::SessionNodeRecord {
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key,
            reason: AgentFrameReason::new("replacement"),
            assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )),
        },
        ..original
    };
    let mut append = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend {
            nodes: vec![replacement],
        },
    );
    append.expected_head_revision = first.head_revision;
    let err = commit_runtime_state_for_test(&store, append, "collision-append")
        .await
        .expect_err("append must reject an id already present in durable history");
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == colliding_id.as_str()
        ),
        "unexpected durable collision error: {err:?}"
    );
    let stored = crate::conformance::helpers::load_one_node(
        store.as_ref(),
        &state.session_id,
        &colliding_id,
    )
    .await
    .expect("original node remains");
    let (reason, _) = stored.frame_open().expect("stored frame");
    assert_eq!(reason.as_str(), "original");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_rejects_duplicate_batch_node_ids(store: Arc<dyn RuntimeStore>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let duplicate_node_id = caller_frame_node_id(&SessionId::from("root"), "duplicate");
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend {
            nodes: vec![
                sample_session_node(&SessionId::from("root"), "duplicate", None),
                sample_session_node(&SessionId::from("root"), "duplicate", None),
            ],
        },
    );
    let err = commit_runtime_state_for_test(&store, commit, "duplicate-batch")
        .await
        .expect_err("a duplicate id in one append must abort the whole commit");
    assert!(
        matches!(
            &err,
            StoreError::NodeIdCollision { node_id } if node_id == duplicate_node_id.as_str()
        ),
        "unexpected duplicate-id error: {err:?}"
    );
    assert!(
        store
            .load_session_window(
                &SessionId::from("root"),
                crate::store::WindowSelector::Current
            )
            .await
            .expect("load after duplicate rejection")
            .is_some_and(|window| window.head_revision == 0),
        "duplicate rejection must leave the created head untouched"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn committed_leaf_is_derived_from_the_terminal_appended_node(
    store: Arc<dyn RuntimeStore>,
) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let first = sample_session_node(&SessionId::from("root"), "append-run", None);
    let second = sample_session_node(
        &SessionId::from("root"),
        "append-leaf",
        Some(first.node_id.as_str()),
    );
    let commit = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend {
            nodes: vec![first, second],
        },
    );
    let expected_leaf = commit
        .graph
        .leaf_node_id()
        .cloned()
        .expect("a non-empty append derives its leaf");
    let receipt = commit_runtime_state_for_test(&store, commit, "derived-leaf")
        .await
        .expect("a well-formed append commits");
    assert_eq!(
        receipt.committed_leaf_node_id.as_ref(),
        Some(&expected_leaf),
        "the committed leaf must be the terminal appended node"
    );
    let loaded = store
        .load_session_window(
            &SessionId::from("root"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load after derived-leaf commit")
        .expect("committed session remains");
    assert_eq!(loaded.window.leaf_node_id.as_ref(), Some(&expected_leaf));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn preserve_head_commit_reports_the_resident_leaf(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let preserve =
        RuntimeCommit::persisted_state_with_graph_commit(&state, crate::GraphAppend::PreserveHead);
    let receipt = store
        .commit_runtime_state(preserve)
        .await
        .expect("a preserve-head append commits without moving the head");
    assert_eq!(
        receipt.committed_leaf_node_id, old_leaf,
        "a preserve-head commit must report the resident leaf"
    );
    let loaded = store
        .load_session_window(
            &SessionId::from("root"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load after preserve-head commit")
        .expect("seeded session remains");
    assert_eq!(loaded.window.leaf_node_id, old_leaf);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn empty_append_cannot_move_the_head(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("empty-append-head-move"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let move_attempt =
        RuntimeCommit::persisted_state_with_graph_commit(&state, crate::GraphAppend::PreserveHead);
    store
        .commit_runtime_state(move_attempt)
        .await
        .expect("an empty append preserves the resident head");
    let loaded = store
        .load_session_window(
            &SessionId::from("empty-append-head-move"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load after preserve-head append")
        .expect("seeded session remains");
    assert_eq!(loaded.window.leaf_node_id, old_leaf);
}
