use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn attachment_manifest_records_intent_and_commit_stamps(
    store: Arc<dyn RuntimePersistence>,
) {
    let committed_by_runtime = AttachmentId::parse("runtime-commit").expect("valid attachment id");
    let committed_out_of_band = AttachmentId::parse("manual-commit").expect("valid attachment id");
    let orphan = AttachmentId::parse("orphan").expect("valid attachment id");
    for id in [&committed_by_runtime, &committed_out_of_band, &orphan] {
        store
            .record_intent(attachment_intent(id.as_str()))
            .expect("record attachment intent");
    }

    let mut uncommitted = store
        .list_uncommitted(200)
        .expect("list uncommitted attachment intents");
    uncommitted.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
    assert_eq!(uncommitted.len(), 3);

    store
        .commit_refs(
            &SessionId::from("root"),
            std::slice::from_ref(&committed_out_of_band),
        )
        .expect("commit attachment ref out of band");
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_committed_attachments([committed_by_runtime.clone()]),
        "attachment-manifest",
    )
    .await
    .expect("runtime commit stamps attachment manifest");

    let still_uncommitted = store
        .list_uncommitted(200)
        .expect("list remaining uncommitted attachments");
    assert_eq!(still_uncommitted.len(), 1);
    assert_eq!(still_uncommitted[0].attachment_id, orphan);
    assert!(still_uncommitted[0].committed_at_epoch_ms.is_none());

    store
        .forget(&SessionId::from("root"), &orphan)
        .expect("forget orphan attachment");
    assert!(
        store
            .list_uncommitted(200)
            .expect("list after forget")
            .is_empty()
    );
}

pub(super) async fn attachment_manifest_keeps_same_content_ownership_per_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let attachment = AttachmentId::parse("same-content").expect("valid attachment id");
    for session_id in ["committed-owner", "orphan-owner"] {
        store
            .record_intent(AttachmentIntent {
                attachment_id: attachment.clone(),
                session_id: SessionId::from(session_id.to_string()),
                canonical_uri: format!("session:{session_id}:sha256:{attachment}"),
                intent_at_epoch_ms: 100,
                owner_kind: None,
                owner_id: None,
            })
            .expect("record independent owner intent");
    }
    store
        .commit_refs(
            &SessionId::from("committed-owner"),
            std::slice::from_ref(&attachment),
        )
        .expect("commit first owner");

    let uncommitted = store.list_uncommitted(200).expect("list owner orphan");
    assert!(
        uncommitted.iter().any(|entry| {
            entry.session_id == "orphan-owner" && entry.attachment_id == attachment
        })
    );
    assert!(!uncommitted.iter().any(|entry| {
        entry.session_id == "committed-owner" && entry.attachment_id == attachment
    }));

    store
        .forget(&SessionId::from("orphan-owner"), &attachment)
        .expect("forget only orphan owner");
    store
        .record_intent(AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: SessionId::from("committed-owner"),
            canonical_uri: format!("session:committed-owner:sha256:{attachment}"),
            intent_at_epoch_ms: 150,
            owner_kind: None,
            owner_id: None,
        })
        .expect("repeat committed owner intent");
    assert!(
        !store
            .list_uncommitted(200)
            .expect("committed ownership remains stamped")
            .iter()
            .any(|entry| entry.session_id == "committed-owner"
                && entry.attachment_id == attachment),
        "a colliding owner or repeated put must not erase another session's commit stamp"
    );
}

pub(super) async fn queued_work_source_keys_are_idempotent_and_list_ordered(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "first",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("enqueue first batch");
    let replay = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("root"),
                "different replay payload",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("source:first"),
        )
        .await
        .expect("replay first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "second",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue second batch");
    store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("other"),
            "other session",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue other session");

    assert_eq!(
        first.batch_id, replay.batch_id,
        "replaying a source key must return the original batch"
    );
    assert_eq!(first.items[0].item_id, replay.items[0].item_id);
    assert_eq!(
        queued_batch_text(&replay),
        Some("first"),
        "source-key replay must return the original stored payload, not the replay attempt"
    );
    let listed = store
        .list_queued_work(&SessionId::from("root"))
        .await
        .expect("list queued work");
    assert_eq!(
        listed
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str(), second.batch_id.as_str()]
    );
    assert!(listed[0].enqueue_seq < listed[1].enqueue_seq);
}

pub(super) async fn concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing(
    store: Arc<dyn RuntimePersistence>,
) {
    let draft = || {
        queued_draft(
            &SessionId::from("concurrent-queued-work-source-key"),
            "concurrent idempotent enqueue",
            DeliveryPolicy::EarliestSafeBoundary,
        )
        .with_source_key("source:concurrent-idempotent-enqueue")
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store.enqueue_queued_work_with_outcome(draft()).await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store.enqueue_queued_work_with_outcome(draft()).await
    });
    barrier.wait().await;
    let left = left
        .await
        .expect("join left concurrent idempotent enqueue")
        .expect("left concurrent idempotent enqueue must not fail");
    let right = right
        .await
        .expect("join right concurrent idempotent enqueue")
        .expect("right concurrent idempotent enqueue must not fail");
    let inserted = [&left, &right]
        .into_iter()
        .filter(|outcome| matches!(outcome, crate::QueuedWorkEnqueueOutcome::Inserted(_)))
        .count();
    let existing = [&left, &right]
        .into_iter()
        .filter(|outcome| matches!(outcome, crate::QueuedWorkEnqueueOutcome::Existing(_)))
        .count();

    assert_eq!(inserted, 1, "exactly one concurrent enqueue must insert");
    assert_eq!(
        existing, 1,
        "exactly one concurrent enqueue must be absorbed"
    );
    assert_eq!(left.batch().batch_id, right.batch().batch_id);
}

pub(super) async fn decorated_queued_work_source_key_replay_reports_absorbed(
    store: Arc<dyn RuntimePersistence>,
) {
    let store = crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(store);
    let draft = || {
        queued_draft(
            &SessionId::from("decorated-queued-work-source-key"),
            "decorated source-key replay",
            DeliveryPolicy::EarliestSafeBoundary,
        )
        .with_source_key("source:decorated-replay")
    };

    let first = store
        .enqueue_queued_work_with_outcome(draft())
        .await
        .expect("enqueue decorated source-key batch");
    assert!(
        matches!(first, crate::QueuedWorkEnqueueOutcome::Inserted(_)),
        "the first decorated enqueue must report inserted"
    );

    let replay = store
        .enqueue_queued_work_with_outcome(draft())
        .await
        .expect("replay decorated source-key batch");
    assert!(
        matches!(replay, crate::QueuedWorkEnqueueOutcome::Existing(_)),
        "the second decorated enqueue must report absorbed"
    );
}

/// The projection reports each family's earliest pending row verbatim, and a
/// timestamp tie between the families resolves to the turn input.
///
/// The tie direction is the portable part. The two families number themselves
/// from independent counters — a PostgreSQL sequence, a SQLite rowid, an
/// in-memory integer — so their `enqueue_seq` values are not comparable across
/// the boundary and nothing here may assert a relationship between them. Only
/// `enqueued_at_ms` may reorder the families; equal timestamps leave the
/// previous winner in place.
pub(super) async fn pending_session_work_ordering_agrees_across_ingress_families(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "pending-work-ordering-tie";
    store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &SessionId::from(session_id),
            &TurnId::from("active-turn"),
            crate::TurnInputCheckpointBoundary::AfterWork,
            "ignored active input",
        ))
        .await
        .expect("seed an active input the next-turn filter must exclude");
    let command = store
        .enqueue_queued_work(queued_session_command_draft(
            &SessionId::from(session_id),
            "command first",
        ))
        .await
        .expect("enqueue command before tied next-turn input");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "input second",
        ))
        .await
        .expect("enqueue tied next-turn input");
    let ordering = store
        .pending_session_work_ordering(&SessionId::from(session_id))
        .await
        .expect("read the tied ordering projection");
    assert_eq!(
        ordering,
        crate::store::PendingSessionWorkOrdering {
            session_command: Some(crate::store::PendingWorkOrderingKey {
                enqueued_at_ms: command.enqueued_at_ms,
                enqueue_seq: command.enqueue_seq,
            }),
            turn_input: Some(crate::store::PendingWorkOrderingKey {
                enqueued_at_ms: input.enqueued_at_ms,
                enqueue_seq: input.enqueue_seq,
            }),
        }
    );
    assert_eq!(
        command.enqueued_at_ms, input.enqueued_at_ms,
        "the tie this case exists to pin must actually be a tie"
    );
    assert!(
        !ordering.session_command_precedes_turn_input(),
        "a timestamp tie must leave the turn input ahead rather than be broken by two \
         independently numbered enqueue sequences"
    );

    let session_id = "pending-work-ordering-command-only";
    let command = store
        .enqueue_queued_work(queued_session_command_draft(
            &SessionId::from(session_id),
            "only a command",
        ))
        .await
        .expect("enqueue a session command with no pending turn input");
    let ordering = store
        .pending_session_work_ordering(&SessionId::from(session_id))
        .await
        .expect("read the command-only ordering projection");
    assert_eq!(
        ordering,
        crate::store::PendingSessionWorkOrdering {
            session_command: Some(crate::store::PendingWorkOrderingKey {
                enqueued_at_ms: command.enqueued_at_ms,
                enqueue_seq: command.enqueue_seq,
            }),
            turn_input: None,
        }
    );
    assert!(ordering.session_command_precedes_turn_input());
}

pub(super) async fn concurrent_queue_and_turn_input_claims_have_one_owner(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = "concurrent-claim-races";
    let batch = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from(session_id),
            "single-owner queue batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue queue batch for claim race");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &SessionId::from(session_id),
            "single-owner turn input",
        ))
        .await
        .expect("enqueue turn input for claim race");
    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from(session_id),
        "claim-race-lease",
    )
    .await;

    let queue_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&queue_barrier);
    let right_barrier = Arc::clone(&queue_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_queue = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &left_fence,
                &lease_owner("queue-left"),
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(1),
            )
            .await
            .map(crate::QueuedWorkClaimOutcome::claim)
    });
    let right_queue = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_ready_queued_work(
                &SessionId::from(session_id),
                &right_fence,
                &lease_owner("queue-right"),
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(1),
            )
            .await
            .map(crate::QueuedWorkClaimOutcome::claim)
    });
    queue_barrier.wait().await;
    let left_queue = left_queue
        .await
        .expect("join left queue claimant")
        .expect("left queue claim race resolves cleanly");
    let right_queue = right_queue
        .await
        .expect("join right queue claimant")
        .expect("right queue claim race resolves cleanly");
    let queue_winners = [left_queue.as_ref(), right_queue.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        queue_winners.len(),
        1,
        "exactly one owner may claim the same queue batch"
    );
    assert_eq!(queue_winners[0].batches[0].batch_id, batch.batch_id);
    assert!(
        store
            .list_pending_queued_work(&SessionId::from(session_id))
            .await
            .expect("list queue after claim race")
            .is_empty(),
        "the winning queue claim must exclusively hide its batch"
    );

    let input_barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&input_barrier);
    let right_barrier = Arc::clone(&input_barrier);
    let left_fence = lease.fence();
    let right_fence = lease.fence();
    let left_input = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store
            .claim_next_turn_inputs(
                &SessionId::from(session_id),
                &left_fence,
                &lease_owner("input-left"),
                1,
            )
            .await
    });
    let right_input = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store
            .claim_next_turn_inputs(
                &SessionId::from(session_id),
                &right_fence,
                &lease_owner("input-right"),
                1,
            )
            .await
    });
    input_barrier.wait().await;
    let left_input = left_input
        .await
        .expect("join left turn-input claimant")
        .expect("left turn-input claim race resolves cleanly");
    let right_input = right_input
        .await
        .expect("join right turn-input claimant")
        .expect("right turn-input claim race resolves cleanly");
    let input_winners = [left_input.as_ref(), right_input.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        input_winners.len(),
        1,
        "exactly one owner may claim the same turn input"
    );
    assert_eq!(input_winners[0].inputs[0].input_id, input.input_id);
    assert_ne!(
        queue_winners[0].owner.owner_id, input_winners[0].owner.owner_id,
        "the queue and turn-input races use independent logical owners"
    );

    release_session_execution_lease_for_test(&store, &lease).await;
}

pub(super) async fn queued_work_cancel_removes_only_unclaimed_batches(
    store: Arc<dyn RuntimePersistence>,
) {
    let cancellable = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "cancel me",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue cancellable batch");
    let cancelled = store
        .cancel_queued_work_batch(&SessionId::from("root"), &cancellable.batch_id)
        .await
        .expect("cancel unclaimed batch")
        .expect("unclaimed batch is returned");
    assert_eq!(cancelled.batch_id, cancellable.batch_id);
    assert_eq!(queued_batch_text(&cancelled), Some("cancel me"));
    assert!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("list after cancellation")
            .is_empty(),
        "cancelled batches must be removed from the durable queue"
    );

    let claimed = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "claimed",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue claimed batch");
    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner").await;
    let claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim batch")
        .claim()
        .expect("claim exists");
    assert_eq!(claim.batches[0].batch_id, claimed.batch_id);
    // The session lease stays live here: a claim is live for lease-less host
    // callers exactly while the generation it pins still holds the session lease
    // (ADR 0029), so the hiding/cancel guards below must observe a live lease.
    assert!(
        store
            .list_pending_queued_work(&SessionId::from("root"))
            .await
            .expect("list pending during active claim")
            .is_empty(),
        "active claims must disappear from user-editable queue snapshots"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("raw durable list during active claim")
            .len(),
        1,
        "claimed batches remain durable until their claim is completed"
    );
    assert!(
        store
            .cancel_queued_work_batch(&SessionId::from("root"), &claimed.batch_id)
            .await
            .expect("cancel active claim")
            .is_none(),
        "actively claimed batches must not be cancelled"
    );
    store
        .abandon_queued_work_claim(&claim)
        .await
        .expect("abandon claim");
    release_session_execution_lease_for_test(&store, &session_lease).await;
    assert_eq!(
        store
            .list_pending_queued_work(&SessionId::from("root"))
            .await
            .expect("list pending after abandoned claim")
            .len(),
        1,
        "abandoned claims become user-editable queue work again"
    );
    assert!(
        store
            .cancel_queued_work_batch(&SessionId::from("root"), &claimed.batch_id)
            .await
            .expect("cancel abandoned claim")
            .is_some(),
        "abandoned batches become cancellable again"
    );
}

pub(super) async fn queued_work_exact_claim_uses_selected_batch_ids(
    store: Arc<dyn RuntimePersistence>,
) {
    let first = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "first",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue first batch");
    let second = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "second",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue second batch");

    let selected_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner").await;
    assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                &SessionId::from("root"),
                &selected_session_lease.fence(),
                &lease_owner("owner"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                std::slice::from_ref(&second.batch_id),
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("boundary-gated exact claim")
            .acquired_no_rows(),
        "exact selection must preserve the delivery boundary gate"
    );
    let exclusive_prefix = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("root"),
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            &[first.batch_id.clone(), second.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim exclusive exact prefix")
        .expect("the first exclusive exact batch is claimable");
    assert_eq!(
        exclusive_prefix
            .batches
            .iter()
            .map(|batch| batch.enqueue_seq)
            .collect::<Vec<_>>(),
        vec![1],
        "exact selection must take only the maximal valid physical prefix"
    );
    store
        .abandon_queued_work_claim(&exclusive_prefix)
        .await
        .expect("abandon exclusive exact-prefix probe");
    let selected = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("root"),
            &selected_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim out-of-order exact batch")
        .expect("selected exact batch exists");
    assert_eq!(selected.batches[0].batch_id, second.batch_id);
    assert_eq!(
        store
            .list_pending_queued_work(&SessionId::from("root"))
            .await
            .expect("list after out-of-order exact claim")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(selected_session_lease.completion())
                .completing_queue_claim(selected.completion()),
        )
        .await
        .expect("complete out-of-order exact batch");

    let accepted_session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner").await;
    let already_settled = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("root"),
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&second.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("resolve already-settled exact batch");
    assert!(
        already_settled.claim.is_none(),
        "an already-settled selected ID must not acquire another claim"
    );
    assert_eq!(
        already_settled.already_satisfied_batch_ids,
        vec![second.batch_id.clone()],
        "an already-settled selected ID is idempotently satisfied"
    );
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("root"),
            &accepted_session_lease.fence(),
            &lease_owner("owner"),
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim first exact batch")
        .expect("first exact claim exists");
    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.batch_id.as_str()]
    );
    // Both selected batches are hidden now: `second` was atomically completed
    // above, while `first` remains held by this live claim.
    assert!(
        store
            .list_pending_queued_work(&SessionId::from("root"))
            .await
            .expect("list pending after exact claim")
            .is_empty()
    );
    release_session_execution_lease_for_test(&store, &accepted_session_lease).await;
}

#[doc(hidden)]
pub async fn queued_work_exact_claim_preserves_physical_order_and_key_breaks(
    store: Arc<dyn RuntimePersistence>,
) {
    let a1 = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("exact-key-break"),
                "a1",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-a1")
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue exact A1");
    let _b1 = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("exact-key-break"),
                "b1",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-b1")
            .with_merge_key("b"),
        )
        .await
        .expect("enqueue exact B1");
    let a2 = store
        .enqueue_queued_work(
            queued_draft(
                &SessionId::from("exact-key-break"),
                "a2",
                DeliveryPolicy::EarliestSafeBoundary,
            )
            .with_source_key("exact-a2")
            .with_merge_key("a"),
        )
        .await
        .expect("enqueue exact A2");

    let owner = lease_owner("exact-key-break-owner");
    let lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("exact-key-break"),
        &owner.owner_id,
    )
    .await;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("exact-key-break"),
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            &[a2.batch_id, a1.batch_id],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim reversed exact A rows")
        .expect("physical prefix contains exact A1");

    assert_eq!(
        claim
            .batches
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("exact-a1"), 1)],
        "an exact claim must preserve enqueue order and stop at the physical B key break"
    );
    assert_eq!(
        store
            .list_pending_queued_work(&SessionId::from("exact-key-break"))
            .await
            .expect("list exact-key-break remainder")
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![(Some("exact-b1"), 2), (Some("exact-a2"), 3)],
        "the key-break row and later requested row must remain queued in physical order"
    );
    release_session_execution_lease_for_test(&store, &lease).await;
}

pub(super) async fn queued_work_classes_gate_command_and_turn_claims(
    store: Arc<dyn RuntimePersistence>,
) {
    let command = store
        .enqueue_queued_work(queued_session_command_draft(
            &SessionId::from("root"),
            "refresh before turn",
        ))
        .await
        .expect("enqueue command");
    let turn = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "user turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue turn");

    let rejected_turn_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "turn-owner")
            .await;
    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from("root"),
                &rejected_turn_lease.fence(),
                &lease_owner("turn-owner"),
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("turn claim with leading command")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::CommandAtHead),
        "turn claims must not skip a leading session command, and every backend \
         must name that same reason"
    );
    release_session_execution_lease_for_test(&store, &rejected_turn_lease).await;

    let command_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "command-owner")
            .await;
    let command_claim = store
        .claim_leading_ready_session_command(
            &SessionId::from("root"),
            &command_lease.fence(),
            &lease_owner("command-owner"),
        )
        .await
        .expect("claim leading command")
        .expect("leading command claim exists");
    assert_eq!(
        command_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![command.batch_id.as_str()]
    );
    let state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(command_lease.completion())
                .completing_queue_claim(command_claim.completion()),
        )
        .await
        .expect("complete command claim");

    let selected_turn_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "turn-owner")
            .await;
    let selected_turn = store
        .claim_ready_queued_work_by_batch_ids(
            &SessionId::from("root"),
            &selected_turn_lease.fence(),
            &lease_owner("turn-owner"),
            QueuedWorkClaimBoundary::Idle,
            &[command.batch_id.clone(), turn.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("resolve mixed command and turn selection after command completion");
    assert_eq!(
        selected_turn.already_satisfied_batch_ids,
        vec![command.batch_id.clone()],
        "the leading command consumed before the selected turn is already satisfied"
    );
    let selected_turn = selected_turn.expect("selected turn claim exists");
    release_session_execution_lease_for_test(&store, &selected_turn_lease).await;
    assert_eq!(selected_turn.batches[0].batch_id, turn.batch_id);

    let first_turn = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("turn-first"),
            "first turn",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue first turn");
    let second_command = store
        .enqueue_queued_work(queued_session_command_draft(
            &SessionId::from("turn-first"),
            "later refresh",
        ))
        .await
        .expect("enqueue later command");
    let rejected_command_lease = claim_session_execution_lease_for_test(
        &store,
        &SessionId::from("turn-first"),
        "command-owner",
    )
    .await;
    assert!(
        store
            .claim_leading_ready_session_command(
                &SessionId::from("turn-first"),
                &rejected_command_lease.fence(),
                &lease_owner("command-owner"),
            )
            .await
            .expect("claim command behind turn")
            .is_none(),
        "session commands must not jump ahead of earlier turn work"
    );
    let turn_claim = store
        .claim_ready_queued_work(
            &SessionId::from("turn-first"),
            &rejected_command_lease.fence(),
            &lease_owner("command-owner"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("claim turn before later command")
        .claim()
        .expect("turn claim exists");
    assert_eq!(turn_claim.batches[0].batch_id, first_turn.batch_id);
    store
        .abandon_queued_work_claim(&turn_claim)
        .await
        .expect("abandon turn claim");
    release_session_execution_lease_for_test(&store, &rejected_command_lease).await;
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("turn-first"))
            .await
            .expect("list turn-first queue")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            first_turn.batch_id.as_str(),
            second_command.batch_id.as_str()
        ]
    );
}

pub(super) async fn queued_work_claims_respect_boundaries_abandon_and_stale_completion(
    store: Arc<dyn RuntimePersistence>,
) {
    let after_commit = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "after current commit",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue after-commit work");
    let earliest = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "earliest",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue earliest work");

    // A single live session lease governs the whole flow: a queued-work claim
    // blocks the checkpoint boundary only while its own generation still holds
    // the session lease (ADR 0029).
    let session_lease =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "owner-a").await;
    assert_eq!(
        store
            .claim_ready_queued_work(
                &SessionId::from("root"),
                &session_lease.fence(),
                &lease_owner("owner-a"),
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("checkpoint claim")
            .refusal(),
        Some(crate::QueuedWorkClaimRefusal::DeliveryBoundaryBlocked),
        "after-current-commit work at the queue head must wait for the idle \
         boundary, and every backend must name that same reason"
    );

    let idle_claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("idle claim")
        .claim()
        .expect("idle claim exists");
    assert_eq!(idle_claim.batches.len(), 1);
    assert_eq!(idle_claim.batches[0].batch_id, after_commit.batch_id);

    // With the after-commit head held by this generation's own live claim, the
    // checkpoint boundary skips past it to the earliest-safe-boundary batch.
    let checkpoint_claim = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("checkpoint claim after head is leased")
        .claim()
        .expect("checkpoint claim exists");
    assert_eq!(checkpoint_claim.batches[0].batch_id, earliest.batch_id);

    // Abandoning the idle claim frees the after-commit batch; reclaiming it under
    // the same live lease advances the fencing token.
    store
        .abandon_queued_work_claim(&idle_claim)
        .await
        .expect("abandon idle claim");
    let reclaimed = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &session_lease.fence(),
            &lease_owner("owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("reclaim abandoned work")
        .claim()
        .expect("reclaimed work exists");
    assert_eq!(reclaimed.batches[0].batch_id, after_commit.batch_id);
    assert!(
        reclaimed.fencing_token > idle_claim.fencing_token,
        "reclaiming abandoned work must advance the fencing token"
    );
    release_session_execution_lease_for_test(&store, &session_lease).await;

    // The pre-abandon claim's completion no longer owns any row: the reclaim
    // rewrote the batch's claim id + lease token, so committing the stale
    // completion is rejected as superseded (ADR 0029 keeps completion validation
    // by claim id + lease token; the abandon+reclaim is what supersedes it).
    let stale_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let stale_err = commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&stale_state, &[])
            .completing_queue_claim(idle_claim.completion()),
        "owner-d",
    )
    .await
    .expect_err("stale pre-abandon completion must be rejected");
    assert!(
        matches!(stale_err, StoreError::QueuedWorkClaimSuperseded { .. }),
        "stale completion produced the wrong error: {stale_err:?}"
    );
    assert_eq!(
        store
            .list_queued_work(&SessionId::from("root"))
            .await
            .expect("rejected stale completion preserves queued work")
            .len(),
        2,
        "rejected stale completion must not delete the reclaimed batch"
    );
}

pub async fn queued_work_claims_supersede_across_session_lease_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: RuntimePersistenceLeaseTiming,
) {
    queued_work_claims_supersede_across_session_lease_generations_with_timing(store, &lease_timing)
        .await;
}

pub(super) async fn queued_work_claims_supersede_across_session_lease_generations_with_timing(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            &SessionId::from("root"),
            "generation work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue generation work");

    // (a) Same generation: a live claim cannot re-claim its own row. The
    // caller's validated-live fence generation matches the row's pinned
    // generation, so self-steal is unrepresentable (ADR 0029).
    let lease_a =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "gen-owner-a")
            .await;
    let claim_a = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &lease_a.fence(),
            &lease_owner("gen-owner-a"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("first-generation claim")
        .claim()
        .expect("first-generation claim exists");
    assert_eq!(claim_a.batches[0].batch_id, batch.batch_id);
    assert_eq!(claim_a.session_lease_generation, lease_a.fencing_token);
    assert!(
        store
            .claim_ready_queued_work(
                &SessionId::from("root"),
                &lease_a.fence(),
                &lease_owner("gen-owner-a"),
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(10),
            )
            .await
            .expect("same-generation re-claim")
            .claim()
            .is_none(),
        "a live claim must not be re-claimable under its own session-lease generation"
    );

    // (b) Release + re-acquire mints a new generation. Re-claiming the batch
    // replaces its ownership and supersedes the old generation's completion.
    release_session_execution_lease_for_test(&store, &lease_a).await;
    let lease_b =
        claim_session_execution_lease_for_test(&store, &SessionId::from("root"), "gen-owner-b")
            .await;
    assert!(
        lease_b.fencing_token > lease_a.fencing_token,
        "re-acquisition must mint a fresh generation"
    );
    let claim_b = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &lease_b.fence(),
            &lease_owner("gen-owner-b"),
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("next-generation reclaim")
        .claim()
        .expect("next-generation reclaim exists");
    assert_eq!(claim_b.batches[0].batch_id, batch.batch_id);
    assert!(claim_b.fencing_token > claim_a.fencing_token);

    let stale_state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let head_before_stale = store
        .load_session()
        .await
        .expect("load head before stale completion");
    let queue_before_stale = store
        .list_queued_work(&SessionId::from("root"))
        .await
        .expect("load queue before stale completion");
    let stale_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_queue_claim(claim_a.completion()),
        )
        .await
        .expect_err("superseded-generation completion must fail");
    assert!(
        matches!(
            stale_err,
            StoreError::QueuedWorkClaimSuperseded {
                ref row_id,
                ref superseding_claim_id,
                ref superseding_session_lease_generation,
                ..
            } if row_id.as_deref() == Some(batch.batch_id.as_str())
                && superseding_claim_id.as_deref() == Some(claim_b.claim_id.as_str())
                && superseding_session_lease_generation.as_deref()
                    == Some(&claim_b.session_lease_generation)
        ),
        "a superseded queued-work completion must report the row and current authority: {stale_err:?}"
    );
    assert_eq!(
        persisted_session_read_snapshot(
            store
                .load_session()
                .await
                .expect("load head after stale completion")
        ),
        persisted_session_read_snapshot(head_before_stale),
        "superseded completion must not mutate the durable head"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .list_queued_work(&SessionId::from("root"))
                .await
                .expect("load queue after stale completion")
        )
        .expect("serialize queue after stale completion"),
        serde_json::to_value(queue_before_stale).expect("serialize queue before stale completion"),
        "superseded completion must not mutate queued work"
    );
    release_session_execution_lease_for_test(&store, &lease_b).await;

    // (c) A TTL takeover mints a new generation without any release. The
    // successor's re-claim below is what supersedes the pre-takeover claim.
    let dead_owner = lease_owner("gen-stale");
    let (dead_lease, claim_dead) = claim_queued_work_under_short_lease(
        &store,
        &SessionId::from("root"),
        &dead_owner,
        lease_timing,
    )
    .await;
    let taker = lease_owner("gen-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        &SessionId::from("root"),
        &taker,
        lease_timing,
        "stale queued-work owner TTL",
    )
    .await;
    assert!(taker_lease.fencing_token > dead_lease.fencing_token);
    let claim_taker = store
        .claim_ready_queued_work(
            &SessionId::from("root"),
            &taker_lease.fence(),
            &taker,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(10),
        )
        .await
        .expect("post-takeover claim")
        .claim()
        .expect("post-takeover claim exists");
    assert_eq!(claim_taker.batches[0].batch_id, batch.batch_id);
    let takeover_err = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&stale_state, &[])
                .completing_queue_claim(claim_dead.completion()),
        )
        .await
        .expect_err("pre-takeover completion must fail");
    assert!(matches!(
        takeover_err,
        StoreError::QueuedWorkClaimSuperseded { .. }
    ));
}

pub(super) fn persisted_session_read_snapshot(
    loaded: Option<crate::store::PersistedSessionRead>,
) -> serde_json::Value {
    loaded.map_or(serde_json::Value::Null, |loaded| {
        let checkpoint = loaded.checkpoint.map(|checkpoint| {
            serde_json::json!({
                "components": checkpoint.components,
            })
        });
        serde_json::json!({
            "head_revision": loaded.head_revision,
            "current_frame_node_id": loaded.current_frame_node_id,
            "graph": loaded.graph,
            "checkpoint_ref": loaded.checkpoint_ref,
            "checkpoint": checkpoint,
            "token_ledger": loaded.token_ledger,
        })
    })
}

pub(super) async fn claim_both_generation_fenced_lanes(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner: &crate::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> (
    QueuedWorkBatch,
    crate::PendingTurnInput,
    crate::SessionExecutionLease,
    crate::QueuedWorkClaim,
    crate::TurnInputClaim,
) {
    let batch = store
        .enqueue_queued_work(queued_draft(
            session_id,
            "lease-less liveness work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue generation-fenced queued work");
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            session_id,
            "lease-less liveness input",
        ))
        .await
        .expect("enqueue generation-fenced turn input");
    let lease = store
        .try_claim_session_execution_lease(
            session_id,
            owner,
            "claim-both-generation-fenced-lanes-executor",
            lease_ttl_ms,
        )
        .await
        .expect("claim session lease for both claim lanes")
        .acquired()
        .expect("session lease for both claim lanes is free");
    let queue_claim = store
        .claim_ready_queued_work(
            session_id,
            &lease.fence(),
            owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(1),
        )
        .await
        .expect("claim generation-fenced queued work")
        .claim()
        .expect("generation-fenced queued work claim exists");
    let input_claim = store
        .claim_next_turn_inputs(session_id, &lease.fence(), owner, 1)
        .await
        .expect("claim generation-fenced turn input")
        .expect("generation-fenced turn input claim exists");
    (batch, input, lease, queue_claim, input_claim)
}

pub(super) async fn assert_both_retained_claims_are_visible_and_cancellable(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    batch: &QueuedWorkBatch,
    input: &crate::PendingTurnInput,
) {
    assert_eq!(
        store
            .list_pending_queued_work(session_id)
            .await
            .expect("list queued work after claim generation stopped being live")
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![batch.batch_id.as_str()],
        "a queued-work claim whose generation is no longer live must be visible"
    );
    assert_eq!(
        store
            .list_pending_turn_inputs(session_id)
            .await
            .expect("list turn inputs after claim generation stopped being live")
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![input.input_id.as_str()],
        "a turn-input claim whose generation is no longer live must be visible"
    );
    let cancelled_batch = store
        .cancel_queued_work_batch(session_id, &batch.batch_id)
        .await
        .expect("cancel queued work after claim generation stopped being live")
        .expect("queued work with a non-live claim generation is cancellable");
    assert_eq!(cancelled_batch.batch_id, batch.batch_id);
    let cancelled_input = store
        .cancel_pending_turn_input(session_id, &input.input_id)
        .await
        .expect("cancel turn input after claim generation stopped being live");
    expect_cancelled_pending_input(cancelled_input, &input.input_id);
}

pub(super) async fn claim_liveness_for_lease_less_paths_tracks_session_generations(
    store: Arc<dyn RuntimePersistence>,
    lease_timing: &RuntimePersistenceLeaseTiming,
) {
    // Release: retain both claim rows, then clear the lease token without
    // abandoning either claim. Lease-less paths must immediately treat both
    // rows as pending again.
    let release_owner = lease_owner("lease-less-release-owner");
    let (batch, input, lease, _queue_claim, _input_claim) = claim_both_generation_fenced_lanes(
        &store,
        &SessionId::from("lease-less-release"),
        &release_owner,
        60_000,
    )
    .await;
    release_session_execution_lease_for_test(&store, &lease).await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        &SessionId::from("lease-less-release"),
        &batch,
        &input,
    )
    .await;

    // Expiry: the lease row still carries the generation, but its token is no
    // longer live once the TTL elapses. The correlated SQL predicates must not
    // mistake generation equality alone for a live claim.
    let expiry_owner = lease_owner("lease-less-expiry-owner");
    let (batch, input, _lease, _queue_claim, _input_claim) = claim_both_generation_fenced_lanes(
        &store,
        &SessionId::from("lease-less-expiry"),
        &expiry_owner,
        lease_timing.scaffolding_lease_ttl_ms(),
    )
    .await;
    lease_timing.wait_until_expired().await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        &SessionId::from("lease-less-expiry"),
        &batch,
        &input,
    )
    .await;

    // TTL takeover advances to a different generation. Claims retained from
    // the expired generation are no longer live for lease-less callers.
    let dead_owner = lease_owner("lease-less-stale");
    let (batch, input, _dead_lease, _queue_claim, _input_claim) =
        claim_both_generation_fenced_lanes(
            &store,
            &SessionId::from("lease-less-takeover"),
            &dead_owner,
            lease_timing.scaffolding_lease_ttl_ms(),
        )
        .await;
    let taker = lease_owner("lease-less-taker");
    let taker_lease = claim_session_execution_lease_after_expiry(
        &store,
        &SessionId::from("lease-less-takeover"),
        &taker,
        lease_timing,
        "lease-less owner TTL",
    )
    .await;
    assert_both_retained_claims_are_visible_and_cancellable(
        &store,
        &SessionId::from("lease-less-takeover"),
        &batch,
        &input,
    )
    .await;
    release_session_execution_lease_for_test(&store, &taker_lease).await;
}
