//! Cold-reopen, session metadata, blob GC and graph-commit conformance laws.
//!
//! Split out of `turn_inputs_and_reopen.rs` to keep that file under the line
//! budget; every law keeps its name and its registration path.

use super::*;
use pretty_assertions::assert_eq;

/// Metadata written through the store round-trips.
///
/// The fixture admitted this session as a root, and the recorded lineage is
/// write-once (FIG-3045), so this law rewrites exactly what the production
/// caller rewrites: the same relation with its pending observer intents
/// settled. Child and fork relations round-trip through
/// `session_store_factory_round_trips_every_relation_shape`, which declares the
/// lineage at admission on the same three backends.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_metadata_round_trips(store: Arc<dyn RuntimePersistence>) {
    let meta = SessionMeta {
        pending_observer_intents: vec![
            crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture("observer-a")),
            crate::SessionObserverIntent::host_requested(crate::ProcessId::fixture("observer-b")),
        ],
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    store
        .save_session_meta(meta.clone())
        .await
        .expect("save session meta");
    let loaded = store
        .load_session_meta()
        .await
        .expect("load session meta")
        .expect("session meta present");
    assert_eq!(loaded, meta);
}

/// The recorded lineage of a session is a durable fact (FIG-1559), so the
/// metadata writer may not quietly replace it.
///
/// `admit_and_bind_session` already refuses a rebind that declares a different
/// lineage. `save_session_meta` replaces the same relation columns, and its one
/// production caller round-trips the metadata it loaded, so a write that
/// carries a different parent, a fork source, or a bare root over a recorded
/// lineage is a rewrite and is refused with
/// [`StoreError::SessionRelationMismatch`](crate::StoreError::SessionRelationMismatch)
/// on every backend, leaving the row untouched. Admission may read
/// [`SessionRelation::Root`] as "no claim"; a write may not, because the row it
/// would record replaces the recorded lineage with that root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_metadata_relation_is_write_once(store: Arc<dyn RuntimePersistence>) {
    // The fixture admitted this session as a root; claiming a parent for it is
    // the conflict `admit_and_bind_session` already refuses on a rebind.
    let recorded = SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("load the admitted session metadata")
            .expect("the fixture admits this session before the law runs"),
        recorded,
        "this law needs the admitted root relation as its precondition"
    );

    // The round trip the production caller performs: the same relation, with
    // its observer intents settled.
    let settled = SessionMeta {
        pending_observer_intents: vec![crate::SessionObserverIntent::host_requested(
            crate::ProcessId::fixture("observer-a"),
        )],
        ..recorded.clone()
    };
    store
        .save_session_meta(settled.clone())
        .await
        .expect("a save that keeps the recorded lineage still writes");

    for (label, relation) in [
        (
            "a parent",
            SessionRelation::Child {
                parent_session_id: SessionId::from("other-parent"),
                caused_by: None,
            },
        ),
        (
            "a fork source",
            SessionRelation::Fork {
                source_session_id: SessionId::from("other-source"),
                source_node_id: crate::NodeId::from("other-node"),
            },
        ),
    ] {
        let error = store
            .save_session_meta(SessionMeta {
                relation,
                ..settled.clone()
            })
            .await
            .expect_err("a metadata write must not rewrite the recorded relation");
        assert!(
            matches!(
                error,
                crate::StoreError::SessionRelationMismatch { ref session_id, .. }
                    if session_id.as_str() == "root"
            ),
            "rewriting the recorded relation to claim {label} must be refused as a relation mismatch, got: {error}"
        );
    }

    assert_eq!(
        store
            .load_session_meta()
            .await
            .expect("load session meta")
            .expect("session meta present"),
        settled,
        "a refused rewrite must leave the recorded metadata untouched"
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
pub async fn gc_blobs(factory: ReopenableRuntimePersistence) {
    let store = factory.open;
    // First commit writes a live checkpoint blob.
    let mut v1 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v1.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(1),
    ));
    let v1_result = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v1, &[]),
        "gc-blobs-v1",
    )
    .await
    .expect("commit v1");
    // Second commit supersedes it with different content, so the v1 checkpoint
    // blob is now unreachable from every session head.
    let mut v2 = RuntimeSessionState {
        session_id: SessionId::from("gc-blobs"),
        head_revision: v1_result.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    v2.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(2),
    ));
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&v2, &[]),
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
        .load_session()
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
pub async fn attachment_manifest_reference_tracking_and_gc_root_set(
    store: Arc<dyn RuntimePersistence>,
) {
    let intent_id = AttachmentId::parse(format!("{:x}", sha256_of(b"intent-only")))
        .expect("valid attachment id");
    let committed_id =
        AttachmentId::parse(format!("{:x}", sha256_of(b"committed"))).expect("valid attachment id");
    let intent = |id: &AttachmentId, at: u64| AttachmentIntent {
        attachment_id: id.clone(),
        session_id: SessionId::from("root"),
        canonical_uri: format!("lash-attachment://blake3/{id}"),
        intent_at_epoch_ms: at,
        owner: None,
    };
    crate::conformance::helpers::record_completed_attachment_write(&store, intent(&intent_id, 100))
        .await;
    crate::conformance::helpers::record_completed_attachment_write(
        &store,
        intent(&committed_id, 100),
    )
    .await;
    store
        .commit_refs(
            &SessionId::from("root"),
            std::slice::from_ref(&committed_id),
        )
        .await
        .expect("commit attachment ref");

    // Root set: every live ref, intent or committed.
    let refs = store.list_all_refs().await.expect("list all refs");
    assert!(refs.contains(&intent_id), "intents feed the GC root set");
    assert!(refs.contains(&committed_id), "commits feed the GC root set");

    // Uncommitted listing still distinguishes intents from commits.
    let uncommitted = store
        .list_uncommitted(1_000_000)
        .await
        .expect("list uncommitted");
    assert!(
        uncommitted
            .iter()
            .any(|entry| entry.attachment_id == intent_id),
        "an uncommitted intent is listed as uncommitted"
    );
    assert!(
        !uncommitted
            .iter()
            .any(|entry| entry.attachment_id == committed_id),
        "a committed attachment is not listed as uncommitted"
    );

    // Forget drops the ref from the root set.
    store
        .forget(&SessionId::from("root"), &intent_id)
        .await
        .expect("forget intent ref");
    assert!(
        !store
            .list_all_refs()
            .await
            .map(|refs| refs.contains(&intent_id))
            .expect("ref dropped"),
        "a forgotten ref is no longer held"
    );
    assert!(
        !store
            .list_all_refs()
            .await
            .expect("list after forget")
            .contains(&intent_id),
        "a forgotten ref leaves the root set"
    );
}

pub(super) fn sha256_of(bytes: &[u8]) -> impl std::fmt::LowerHex {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_receipt_reopen(factory: ReopenableRuntimePersistence) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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

    let mut reopened_state = loaded_conformance_state(&factory.reopen).await;
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
pub async fn runtime_reopen(factory: ReopenableRuntimePersistence) {
    session_execution_lease_first_claim_excludes_concurrent_reopen_handles(&factory).await;

    let meta = SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    factory
        .open
        .save_session_meta(meta.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(77),
    ));
    let initial_commit = commit_runtime_state_for_test(
        &factory.open,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "reopen",
    )
    .await
    .expect("commit state");
    state.head_revision = initial_commit.head_revision;

    let application_lease = claim_session_execution_lease_for_test(
        &factory.open,
        &SessionId::from("root"),
        "reopen-applications",
    )
    .await;
    let mut expected_applications = Vec::new();
    for (turn_index, turn_id) in ["z-reopen-application", "a-reopen-application"]
        .into_iter()
        .enumerate()
    {
        factory
            .open
            .enqueue_pending_turn_input(
                pending_next_turn_input_draft(
                    &SessionId::from("root"),
                    &format!("reopen application {turn_index}"),
                )
                .with_source_key(format!("host:reopen-application-{turn_index}")),
            )
            .await
            .expect("enqueue reopen application");
        let mut claim = factory
            .open
            .claim_next_turn_inputs(
                &SessionId::from("root"),
                &application_lease.fence(),
                &lease_owner("reopen-applications"),
                1,
            )
            .await
            .expect("claim reopen application")
            .expect("reopen application claim");
        claim.record_initial_turn_application(
            &crate::TurnId::from(turn_id),
            &format!("reopen-application-message-{turn_index}"),
        );
        expected_applications.extend(claim.applications.clone());

        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(claim.completion());
        if turn_index == 1 {
            commit = commit.releasing_session_execution_lease(application_lease.completion());
        }
        commit.turn_commit =
            RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", turn_id, "final"));
        let result = factory
            .open
            .commit_runtime_state(commit)
            .await
            .expect("commit reopen application");
        state.head_revision = result.head_revision;
    }
    let queued = factory
        .open
        .enqueue_queued_work(keyed_queued_draft(
            &SessionId::from("root"),
            "survives reopen",
            DeliveryPolicy::EarliestSafeBoundary,
            "reopen:queued",
        ))
        .await
        .expect("enqueue queued work");
    let attachment = AttachmentId::parse("reopen-attachment").expect("valid attachment id");
    crate::conformance::helpers::record_completed_attachment_write(
        &factory.open,
        AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: SessionId::from("root"),
            canonical_uri: "sha256:reopen-attachment".to_string(),
            intent_at_epoch_ms: 100,
            owner: None,
        },
    )
    .await;

    let reopened_meta = factory
        .reopen
        .load_session_meta()
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta, meta);
    let reopened = factory
        .reopen
        .load_session()
        .await
        .expect("load reopened state")
        .expect("reopened state");
    assert_eq!(reopened.session_id, "root");
    assert_eq!(
        reopened
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| {
                checkpoint
                    .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
                    .expect("decode reopened tool state")
            })
            .map(|tool_state| tool_state.generation()),
        Some(77)
    );
    assert_eq!(
        factory
            .reopen
            .list_turn_input_applications(&SessionId::from("root"))
            .await
            .expect("list applications from reopened handle"),
        expected_applications,
        "a fresh durable handle must reconcile applications in turn-commit order"
    );
    let reopened_queue = factory
        .reopen
        .list_queued_work(&SessionId::from("root"))
        .await
        .expect("list reopened queue");
    assert_eq!(reopened_queue.len(), 1);
    assert_eq!(reopened_queue[0].batch_id, queued.batch_id);
    assert_eq!(
        queued_batch_text(&reopened_queue[0]),
        Some("survives reopen")
    );
    let reopened_intents = factory
        .reopen
        .list_uncommitted(200)
        .await
        .expect("list reopened attachment intents");
    assert!(
        reopened_intents
            .iter()
            .any(|intent| intent.attachment_id == attachment),
        "attachment intent rows must survive reopening a durable store"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_execution_lease_first_claim_excludes_concurrent_reopen_handles(
    factory: &ReopenableRuntimePersistence,
) {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let open = Arc::clone(&factory.open);
    let reopen = Arc::clone(&factory.reopen);
    let open_barrier = Arc::clone(&barrier);
    let reopen_barrier = Arc::clone(&barrier);
    let open_owner = lease_owner("owner-a");
    let reopen_owner = lease_owner("owner-b");

    let open_claim = crate::task::spawn(async move {
        open_barrier.wait().await;
        open.try_claim_session_execution_lease(
            &SessionId::from("first-claim-race"),
            &open_owner,
            "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor",
            60_000,
        )
        .await
    });
    let reopen_claim = crate::task::spawn(async move {
        reopen_barrier.wait().await;
        reopen
            .try_claim_session_execution_lease(
                &SessionId::from("first-claim-race"),
                &reopen_owner,
                "session-execution-lease-first-claim-excludes-concurrent-reopen-handles-executor-2",
                60_000,
            )
            .await
    });

    barrier.wait().await;
    let open_claim = open_claim
        .await
        .expect("join open first-claim race")
        .expect("open first-claim race");
    let reopen_claim = reopen_claim
        .await
        .expect("join reopen first-claim race")
        .expect("reopen first-claim race");
    let open_lease = open_claim.acquired();
    let reopen_lease = reopen_claim.acquired();
    let claim_count = usize::from(open_lease.is_some()) + usize::from(reopen_lease.is_some());
    assert_eq!(
        claim_count, 1,
        "exactly one concurrent first claim may acquire a session execution lease"
    );
    if let Some(lease) = open_lease.as_ref().or(reopen_lease.as_ref()) {
        factory
            .open
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release first-claim race winner");
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_wake_delivery_is_source_key_idempotent_and_claimed_once(
    store: Arc<dyn RuntimePersistence>,
) {
    let wake = root_process_wake(7);
    let malformed = QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        crate::TurnWorkPayload::process_wake(wake.clone()),
    )
    .with_source_key(crate::process_wake_source_key(
        &wake.process_id,
        wake.sequence,
    ));
    store
        .enqueue_queued_work(malformed)
        .await
        .expect_err("process-wake enqueue must require structural producer identity");

    let first = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("enqueue wake");
    let replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("replay wake enqueue");
    assert_eq!(
        first.batch_id, replay.batch_id,
        "wake source-key replay must return the original queued batch"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list queued wakes")
            .len(),
        1,
        "replayed wake must not create a second queued delivery"
    );

    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "wake-owner")
            .await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("wake-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim wake")
        .claim()
        .expect("wake claim");
    assert_eq!(claim.batches.len(), 1);
    assert_eq!(claim.batches[0].items.len(), 1);
    assert!(matches!(
        claim.batches[0].items[0].payload,
        QueuedWorkPayload::ProcessWake { .. }
    ));
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(session_lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("wake delivery completion commits");
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after wake completion")
            .is_empty(),
        "completed wake delivery must be removed exactly once"
    );
    let consumed_replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake))
        .await
        .expect_err("late no-live-row wake must trip the receiver floor");
    assert!(matches!(
        consumed_replay,
        StoreError::ProcessWakeSequenceRewound { .. }
    ));
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after consumed wake redelivery")
            .is_empty(),
        "receiver evidence must prevent a late redelivery from recreating queued work"
    );
}

/// A process wake from `process-1` to `root` at `sequence`.
fn root_process_wake(sequence: u64) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("wake-{sequence}"),
        target_session_id: SessionId::from("root"),
        process_id: crate::ProcessId::fixture("process-1"),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: RuntimeInvocation {
            attribution: RuntimeAttribution::for_session("root"),
            subject: RuntimeSubject::ProcessEvent {
                process_id: crate::ProcessId::fixture("process-1"),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: "wake payload".to_string(),
        created_at_ms: 1,
    }
}

/// A host cancel is a terminal transition of a wake, so it raises the
/// session's redelivery floor in the same transaction that removes the row
/// (FIG-3545). A redelivery of the withdrawn `(process, seq)` — after a
/// producer crash, a failed terminal mark or a lost claim — is refused with
/// the typed rewind outcome instead of resurrecting the wake; a later
/// sequence from the same process is still admitted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_cancelled_wake_is_not_redelivered(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("root");
    let queued = store
        .enqueue_queued_work(crate::process_wake_batch_draft(root_process_wake(7)))
        .await
        .expect("enqueue wake");
    store
        .cancel_queued_work_batch(&session_id, &queued.batch_id)
        .await
        .expect("host cancel of the queued wake")
        .expect("an unclaimed wake is cancelled");

    let redelivery = store
        .enqueue_queued_work(crate::process_wake_batch_draft(root_process_wake(7)))
        .await
        .expect_err("redelivery of a host-cancelled wake must trip the receiver floor");
    match redelivery {
        StoreError::ProcessWakeSequenceRewound {
            session_id: refused_session,
            process_id,
            sequence,
            allocation_floor,
        } => {
            assert_eq!(refused_session, session_id);
            assert_eq!(process_id, crate::ProcessId::fixture("process-1"));
            assert_eq!(sequence, 7);
            assert_eq!(allocation_floor, 7);
        }
        other => panic!("expected ProcessWakeSequenceRewound, got {other:?}"),
    }
    assert!(
        store
            .list_queued_work(&session_id)
            .await
            .expect("list after refused redelivery")
            .is_empty(),
        "a host-cancelled wake must not come back"
    );

    let later = store
        .enqueue_queued_work(crate::process_wake_batch_draft(root_process_wake(8)))
        .await
        .expect("a later sequence stays above the floor");
    assert_eq!(
        store
            .list_queued_work(&session_id)
            .await
            .expect("list after later wake")
            .into_iter()
            .map(|batch| batch.batch_id)
            .collect::<Vec<_>>(),
        vec![later.batch_id],
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn final_commit_stamp_is_idempotent_and_conflicts_on_changed_hash(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let graph_data = state.session_graph.data_mut();
    std::sync::Arc::make_mut(&mut graph_data.nodes[0]).timestamp =
        "2026-07-26T10:00:00Z".to_string();
    state.set_execution_state_snapshot(Some(vec![7; 1_024].into()));
    let operation = crate::OperationId::turn("root", "provider-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp first commit");
    let turn_commit_hash = stamped_commit
        .turn_commit_hash()
        .expect("first commit hash");

    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "provider-turn")
            .await;
    let first = store
        .commit_runtime_state(
            stamped_commit
                .clone()
                .releasing_session_execution_lease(session_lease.completion()),
        )
        .await
        .expect("first final commit requires a live session execution lease");
    let mut replay_state = state.clone();
    let replay_graph_data = replay_state.session_graph.data_mut();
    std::sync::Arc::make_mut(&mut replay_graph_data.nodes[0]).timestamp =
        "2026-07-26T10:00:09Z".to_string();
    let (replay_commit, _) = RuntimeCommit::persisted_state_for_test(&replay_state, &[])
        .with_operation(operation.clone())
        .expect("derive and stamp replay");
    let replay_hash = replay_commit
        .turn_commit_hash()
        .expect("replay commit hash");
    assert_eq!(replay_hash, turn_commit_hash);
    let retry = store
        .commit_runtime_state(replay_commit)
        .await
        .expect("same final commit retries idempotently without a live lease");
    assert_eq!(retry.head_revision, first.head_revision);
    assert_eq!(retry.checkpoint_ref, first.checkpoint_ref);
    let receipt_json = serde_json::to_string(&first).expect("serialize commit receipt");
    assert!(
        !receipt_json.contains("execution_state_snapshot"),
        "commit receipts must retain frame references and timestamps, never snapshot bytes"
    );
    replay_state.apply_persisted_commit_result(retry.clone());

    let mut retry_from_new_head = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation.clone())
        .expect("stamp retry from advanced head")
        .0;
    retry_from_new_head.expected_head_revision = first.head_revision;
    let retry_hash = retry_from_new_head
        .turn_commit_hash()
        .expect("retry commit hash");
    assert_eq!(
        retry_hash, turn_commit_hash,
        "turn commit identity must not depend on the optimistic CAS revision"
    );

    let changed_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 1,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let mut changed = RuntimeCommit::persisted_state_for_test(&changed_state, &[]);
    changed.turn_commit =
        RuntimeTurnCommitStamp::new(crate::OperationId::turn("root", "provider-turn", "final"));
    let err = store
        .commit_runtime_state(changed)
        .await
        .expect_err("same provider turn id with a different commit hash must conflict");
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected changed-hash error: {err:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn store_computed_hash_rejects_mutated_commit(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "realization-guard", "final");
    let frame_key = crate::FrameKey::from_caller_material("realization-guard-frame")
        .expect("non-empty frame material");
    let node_id = crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    let graph = crate::GraphAppend::Extend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: node_id.to_string().into(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::FrameOpen {
                frame_key,
                reason: AgentFrameReason::initial(),
                assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                )),
                protocol_turn_options: ProtocolTurnOptions::default(),
            },
        }],
    };
    let (first, node_id_mapping) =
        RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[])
            .with_operation(operation)
            .expect("stamp guarded commit");
    assert_eq!(
        node_id_mapping,
        vec![(
            lash_core::NodeId::new(node_id.as_str()),
            lash_core::NodeId::new(node_id.as_str()),
        )],
        "operation stamping must return the append-id mapping"
    );
    commit_runtime_state_for_test(&store, first.clone(), "realization-guard")
        .await
        .expect("first guarded commit");

    let first_hash = first.turn_commit_hash().expect("first store-computed hash");
    let mut divergent_replay = first;
    let nodes = divergent_replay.graph.nodes_mut();
    nodes[0].parent_node_id = Some("proposal-only-parent".into());
    let divergent_hash = divergent_replay
        .turn_commit_hash()
        .expect("mutated store-computed hash");
    assert_ne!(
        divergent_hash, first_hash,
        "the receipt identity must cover mutated topology"
    );
    let err = crate::store::commit_runtime_state_verified(store.as_ref(), divergent_replay)
        .await
        .expect_err("the store must reject a mutated commit reusing an operation id");
    assert!(
        matches!(&err, StoreError::RuntimeTurnCommitConflict { .. }),
        "unexpected mutated-commit error: {err:?}"
    );
    let stored = store
        .load_node(&node_id)
        .await
        .expect("load guarded node")
        .expect("guarded node remains stored");
    assert_eq!(
        stored.parent_node_id, None,
        "a rejected receipt replay must not adopt or persist proposal topology"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_rejects_non_derived_append_node_ids(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let operation = crate::OperationId::turn("root", "guard-turn", "final");
    let graph = crate::GraphAppend::Extend {
        nodes: vec![crate::SessionNodeRecord {
            node_id: "rogue-node-id".into(),
            parent_node_id: None,
            timestamp: "2026-07-26T10:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "guard".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"ok": true})),
            },
        }],
    };
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit(&state, graph, &[]);
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
            .load_session()
            .await
            .expect("load after guard rejection")
            .is_none(),
        "guard rejection must happen before any durable write"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_rejects_existing_node_id_collision(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let frame_key =
        crate::FrameKey::from_caller_material("collision-frame").expect("non-empty frame material");
    let colliding_id = crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());
    let original = crate::SessionNodeRecord {
        node_id: colliding_id.to_string().into(),
        parent_node_id: None,
        timestamp: "2026-07-26T10:00:00Z".to_string(),
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: frame_key.clone(),
            reason: AgentFrameReason::new("original"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
    };
    state.session_graph = crate::SessionGraph::from_nodes(
        vec![original.clone()],
        Some(colliding_id.to_string().into()),
    )
    .expect("collision fixture seed graph is valid");
    let initial = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let first = commit_runtime_state_for_test(&store, initial, "collision-seed")
        .await
        .expect("seed colliding durable node");

    let replacement = crate::SessionNodeRecord {
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key,
            reason: AgentFrameReason::new("replacement"),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: ProtocolTurnOptions::default(),
        },
        ..original
    };
    let mut append = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::Extend {
            nodes: vec![replacement],
        },
        &[],
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
    let stored = store
        .load_node(&colliding_id)
        .await
        .expect("load original node")
        .expect("original node remains");
    let (reason, _, _) = stored.frame_open().expect("stored frame");
    assert_eq!(reason.as_str(), "original");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn append_rejects_duplicate_batch_node_ids(store: Arc<dyn RuntimePersistence>) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        &[],
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
            .load_session()
            .await
            .expect("load after duplicate rejection")
            .is_none(),
        "duplicate rejection must happen before any durable write"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn committed_leaf_is_derived_from_the_terminal_appended_node(
    store: Arc<dyn RuntimePersistence>,
) {
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let first = sample_session_node(&SessionId::from("root"), "append-root", None);
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
        &[],
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
        .load_session()
        .await
        .expect("load after derived-leaf commit")
        .expect("committed session remains");
    assert_eq!(loaded.graph.leaf_node_id.as_ref(), Some(&expected_leaf));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn preserve_head_commit_reports_the_resident_leaf(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let preserve = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::PreserveHead,
        &[],
    );
    let receipt = store
        .commit_runtime_state(preserve)
        .await
        .expect("a preserve-head append commits without moving the head");
    assert_eq!(
        receipt.committed_leaf_node_id, old_leaf,
        "a preserve-head commit must report the resident leaf"
    );
    let loaded = store
        .load_session()
        .await
        .expect("load after preserve-head commit")
        .expect("seeded session remains");
    assert_eq!(loaded.graph.leaf_node_id, old_leaf);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn empty_append_cannot_move_the_head(store: Arc<dyn RuntimePersistence>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("empty-append-head-move"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let first = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed the live head");
    let old_leaf = state.session_graph.leaf_node_id.clone();
    state.apply_persisted_commit_result(first);
    let mut move_attempt = RuntimeCommit::persisted_state_with_graph_commit(
        &state,
        crate::GraphAppend::PreserveHead,
        &[],
    );
    move_attempt.current_frame_node_id = old_leaf.clone().map(|frame_node_id| {
        crate::FrameNodeId::new(frame_node_id).expect("test frame identity is non-empty")
    });
    store
        .commit_runtime_state(move_attempt)
        .await
        .expect("an empty append preserves the resident head");
    let loaded = store
        .load_session()
        .await
        .expect("load after preserve-head append")
        .expect("seeded session remains");
    assert_eq!(loaded.graph.leaf_node_id, old_leaf);
}
