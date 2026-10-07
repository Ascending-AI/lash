use super::*;
use pretty_assertions::assert_eq;

#[expect(clippy::expect_used, reason = "conformance setup")]
pub async fn attachment_writes_keep_independent_referrers(store: Arc<dyn RuntimeStore>) {
    let id = AttachmentId::parse("independent-reference").expect("id");
    let a = crate::ArtifactReferrer::ProcessRecord(crate::ProcessId::fixture("a"));
    let b = crate::ArtifactReferrer::ProcessRecord(crate::ProcessId::fixture("b"));
    for referrer in [&a, &b] {
        crate::conformance::helpers::record_completed_attachment_write(
            &store,
            crate::AttachmentWrite {
                attachment_id: id.clone(),
                claim: crate::ReferrerClaim::unguarded(referrer.clone()).expect("claim"),
            },
        )
        .await;
    }
    store.end_attachment_referrer(&a).await.expect("end a");
    assert_eq!(
        store.attachment_referrers(&id).await.expect("refs"),
        vec![b]
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_source_keys_are_idempotent_and_list_ordered(store: Arc<dyn RuntimeStore>) {
    let first = store
        .enqueue_queued_work(keyed_queued_draft(
            &SessionId::from("root"),
            "first",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:first",
        ))
        .await
        .expect("enqueue first batch");
    let replay = store
        .enqueue_queued_work(keyed_queued_draft(
            &SessionId::from("root"),
            "first",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:first",
        ))
        .await
        .expect("replay first batch");
    let changed = store
        .enqueue_queued_work(keyed_queued_draft(
            &SessionId::from("root"),
            "different replay payload",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:first",
        ))
        .await
        .expect_err("a changed submission under the source key is refused");
    assert!(
        matches!(
            &changed,
            StoreError::QueuedWorkSourceKeyConflict { existing_batch_id, .. }
                if *existing_batch_id == first.batch_id
        ),
        "a changed submission names the stored batch: {changed:?}"
    );
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
    assert_eq!(queued_batch_text(&replay), Some("first"));
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing(
    store: Arc<dyn RuntimeStore>,
) {
    let draft = || {
        keyed_queued_draft(
            &SessionId::from("concurrent-queued-work-source-key"),
            "concurrent idempotent enqueue",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:concurrent-idempotent-enqueue",
        )
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn decorated_queued_work_source_key_replay_reports_absorbed(
    store: Arc<dyn RuntimeStore>,
) {
    let store = crate::testing::checkpoint_observer::fresh_runtime_persistence_handle(store);
    let draft = || {
        keyed_queued_draft(
            &SessionId::from("decorated-queued-work-source-key"),
            "decorated source-key replay",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:decorated-replay",
        )
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

/// Commands precede turn inputs at a boundary, including timestamp ties.
/// Both producers draw from the same session sequence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_session_work_ordering_agrees_across_ingress_families(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = "pending-work-ordering-tie";
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
        ordering.session_command_precedes_turn_input(),
        "commands drain before turn inputs even when their timestamps tie"
    );

    assert!(command.enqueue_seq < input.enqueue_seq);

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
