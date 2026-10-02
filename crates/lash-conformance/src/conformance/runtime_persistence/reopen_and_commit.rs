//! Cold-reopen, session metadata, blob GC and graph-commit conformance laws.
//!
//! Split out of `turn_inputs_and_reopen.rs` to keep that file under the line
//! budget; every law keeps its name and its registration path.

use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
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
pub async fn runtime_reopen(factory: ReopenableRuntimeStore) {
    let meta = SessionMeta {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("root"),
        relation: SessionRelation::Root,
    };
    factory
        .open
        .settle_observer_intents(&meta.session_id, meta.pending_observer_intents.clone())
        .await
        .expect("save meta");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(77),
    ));
    let initial_commit = commit_runtime_state_for_test(
        &factory.open,
        RuntimeCommit::persisted_state_for_test(&state),
        "reopen",
    )
    .await
    .expect("commit state");
    state.head_revision = initial_commit.head_revision;

    let application_lease = seal_drive_fence_for_test(
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
        let head = factory
            .open
            .list_pending_turn_inputs(&SessionId::from("root"))
            .await
            .expect("list the reopen application")
            .remove(0)
            .input;
        let admission = admitted_root(
            &factory.open,
            &application_lease,
            turn_id,
            lash_core::store::AdmittedHead::Input(head.input_id),
        )
        .await;
        let mut admitted = *admission.inputs.expect("the root admits its input");
        admitted.record_initial_turn_application(
            &crate::TurnId::from(turn_id),
            &format!("reopen-application-message-{turn_index}"),
        );
        expected_applications.extend(admitted.applications.clone());

        let mut settlement = lash_core::store::IngressSettlement::new(TurnId::from(turn_id));
        settlement.completed_inputs.push(admitted.completion());
        let mut commit = final_commit(
            RuntimeCommit::persisted_state_for_test(&state),
            &application_lease,
            settlement,
        );
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
        crate::AttachmentWrite {
            attachment_id: attachment.clone(),
            claim: crate::conformance::attachment_referrers::claim(
                crate::ArtifactReferrer::Session(SessionId::from("root")),
            ),
        },
    )
    .await;

    let reopened_meta = factory
        .reopen
        .load_session_meta(&SessionId::from("root"))
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta, meta);
    let reopened = factory
        .reopen
        .load_session_window(
            &SessionId::from("root"),
            crate::store::WindowSelector::Current,
        )
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
    assert_eq!(
        factory
            .reopen
            .attachment_referrers(&attachment)
            .await
            .expect("reopened refs"),
        vec![crate::ArtifactReferrer::Session("root".into())]
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_wake_delivery_is_source_key_idempotent_and_admitted_once(
    store: Arc<dyn RuntimeStore>,
) {
    let wake = root_process_wake(7);
    let malformed = QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        crate::QueuedWorkPayload::process_wake(wake.clone()),
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
        seal_drive_fence_for_test(&store, &SessionId::from("root"), "wake-owner").await;
    let admission = admitted_root(
        &store,
        &session_lease,
        "wake-root",
        lash_core::store::AdmittedHead::Batch(first.batch_id.clone()),
    )
    .await;
    let admitted = admission.queued.as_ref().expect("the root admits the wake");
    assert_eq!(admitted.batches.len(), 1);
    assert!(matches!(
        admitted.batches[0].payload,
        QueuedWorkPayload::ProcessWake { .. }
    ));
    end_root(
        &store,
        &session_lease,
        completing_admission("wake-root", &admission),
    )
    .await;
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after wake completion")
            .is_empty(),
        "completed wake delivery must be removed exactly once"
    );
    // Until vacuum the delivered tombstone answers a late redelivery.
    let answered = store
        .enqueue_queued_work_with_outcome(crate::process_wake_batch_draft(wake.clone()))
        .await
        .expect("a late redelivery answers the delivered tombstone");
    assert!(
        matches!(
            &answered,
            crate::QueuedWorkEnqueueOutcome::Existing(batch)
                if batch.batch_id == first.batch_id
                    && batch.terminal.as_ref().map(|terminal| terminal.cause)
                        == Some(lash_core::store::IngressTerminalCause::Delivered)
        ),
        "the late redelivery is the delivered wake: {answered:?}"
    );
    store
        .vacuum(&SessionId::from("root"))
        .await
        .expect("vacuum the delivered tombstone");
    let consumed_replay = store
        .enqueue_queued_work(crate::process_wake_batch_draft(wake))
        .await
        .expect_err("a vacuumed wake's late redelivery must trip the receiver floor");
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
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
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
/// session's redelivery floor in the same transaction that leaves the
/// `cancelled` tombstone (FIG-3545, ADR 0101 §8). A redelivery of the
/// withdrawn `(process, seq)` — after a producer crash, a failed terminal
/// mark or a lost admission — answers the tombstone until host vacuum and is
/// refused with the typed rewind outcome after it, never resurrecting the
/// wake; a later sequence from the same process is still admitted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_cancelled_wake_is_not_redelivered(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("root");
    let queued = store
        .enqueue_queued_work(crate::process_wake_batch_draft(root_process_wake(7)))
        .await
        .expect("enqueue wake");
    store
        .cancel_queued_work_batch(&session_id, &queued.batch_id)
        .await
        .expect("host cancel of the queued wake")
        .expect("an unadmitted wake is cancelled");

    let answered = store
        .enqueue_queued_work_with_outcome(crate::process_wake_batch_draft(root_process_wake(7)))
        .await
        .expect("a redelivery answers the cancelled tombstone");
    assert!(
        matches!(
            &answered,
            crate::QueuedWorkEnqueueOutcome::Existing(batch)
                if batch.batch_id == queued.batch_id
                    && batch.terminal.as_ref().map(|terminal| terminal.cause)
                        == Some(lash_core::store::IngressTerminalCause::Cancelled)
        ),
        "the redelivery is the cancelled wake: {answered:?}"
    );
    assert!(
        store
            .list_queued_work(&session_id)
            .await
            .expect("list after answered redelivery")
            .is_empty(),
        "an answered redelivery reopens nothing"
    );
    store
        .vacuum(&session_id)
        .await
        .expect("vacuum the cancelled tombstone");
    let redelivery = store
        .enqueue_queued_work(crate::process_wake_batch_draft(root_process_wake(7)))
        .await
        .expect_err("redelivery of a vacuumed host-cancelled wake must trip the receiver floor");
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
    store: Arc<dyn RuntimeStore>,
) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let graph_data = state.session_graph.data_mut();
    std::sync::Arc::make_mut(&mut graph_data.nodes.make_mut()[0]).timestamp =
        "2026-07-26T10:00:00Z".to_string();
    state.set_execution_state_snapshot(Some(vec![7; 1_024].into()));
    let operation = crate::OperationId::turn("root", "provider-turn", "final");
    let (stamped_commit, _) = RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(operation.clone())
        .expect("derive and stamp first commit");
    let turn_commit_hash = stamped_commit
        .turn_commit_hash()
        .expect("first commit hash");

    let _session_lease =
        seal_drive_fence_for_test(&store, &SessionId::from("root"), "provider-turn").await;
    let first = store
        .commit_runtime_state(stamped_commit.clone())
        .await
        .expect("first final commit uses the sealed drive epoch");
    let mut replay_state = state.clone();
    let replay_graph_data = replay_state.session_graph.data_mut();
    std::sync::Arc::make_mut(&mut replay_graph_data.nodes.make_mut()[0]).timestamp =
        "2026-07-26T10:00:09Z".to_string();
    let (replay_commit, _) = RuntimeCommit::persisted_state_for_test(&replay_state)
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

    let mut retry_from_new_head = RuntimeCommit::persisted_state_for_test(&state)
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let mut changed = RuntimeCommit::persisted_state_for_test(&changed_state);
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
pub async fn store_computed_hash_rejects_mutated_commit(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
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
                assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )),
            },
        }],
    };
    let (first, node_id_mapping) = RuntimeCommit::persisted_state_with_graph_commit(&state, graph)
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
    let stored = crate::conformance::helpers::load_one_node(
        store.as_ref(),
        &SessionId::from("root"),
        &node_id,
    )
    .await
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
            timestamp: "2026-07-26T10:00:00Z".to_string(),
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
        node_id: colliding_id.to_string().into(),
        parent_node_id: None,
        timestamp: "2026-07-26T10:00:00Z".to_string(),
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
        Some(colliding_id.to_string().into()),
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
    let mut move_attempt =
        RuntimeCommit::persisted_state_with_graph_commit(&state, crate::GraphAppend::PreserveHead);
    move_attempt.current_frame_node_id = old_leaf.clone().map(|frame_node_id| {
        crate::FrameNodeId::new(frame_node_id).expect("test frame identity is non-empty")
    });
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
