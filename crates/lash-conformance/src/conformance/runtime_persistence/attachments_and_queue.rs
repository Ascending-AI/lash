use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn attachment_manifest_records_intent_and_commit_stamps(
    store: Arc<dyn RuntimePersistence>,
) {
    let committed_by_runtime = AttachmentId::parse("runtime-commit").expect("valid attachment id");
    let committed_out_of_band = AttachmentId::parse("manual-commit").expect("valid attachment id");
    let orphan = AttachmentId::parse("orphan").expect("valid attachment id");
    for id in [&committed_by_runtime, &committed_out_of_band, &orphan] {
        crate::conformance::helpers::record_completed_attachment_write(
            &store,
            attachment_intent(id.as_str()),
        )
        .await;
    }

    let mut uncommitted = store
        .list_uncommitted(200)
        .await
        .expect("list uncommitted attachment intents");
    uncommitted.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
    assert_eq!(uncommitted.len(), 3);

    store
        .commit_refs(
            &SessionId::from("root"),
            std::slice::from_ref(&committed_out_of_band),
        )
        .await
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
        .await
        .expect("list remaining uncommitted attachments");
    assert_eq!(still_uncommitted.len(), 1);
    assert_eq!(still_uncommitted[0].attachment_id, orphan);
    assert!(still_uncommitted[0].committed_at_epoch_ms.is_none());

    store
        .forget(&SessionId::from("root"), &orphan)
        .await
        .expect("forget orphan attachment");
    assert!(
        store
            .list_uncommitted(200)
            .await
            .expect("list after forget")
            .is_empty()
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn attachment_manifest_keeps_same_content_ownership_per_session(
    store: Arc<dyn RuntimePersistence>,
) {
    let attachment = AttachmentId::parse("same-content").expect("valid attachment id");
    for session_id in ["committed-owner", "orphan-owner"] {
        crate::conformance::helpers::record_completed_attachment_write(
            &store,
            AttachmentIntent {
                attachment_id: attachment.clone(),
                session_id: SessionId::from(session_id.to_string()),
                canonical_uri: format!("session:{session_id}:sha256:{attachment}"),
                intent_at_epoch_ms: 100,
                owner: None,
            },
        )
        .await;
    }
    store
        .commit_refs(
            &SessionId::from("committed-owner"),
            std::slice::from_ref(&attachment),
        )
        .await
        .expect("commit first owner");

    let uncommitted = store
        .list_uncommitted(200)
        .await
        .expect("list owner orphan");
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
        .await
        .expect("forget only orphan owner");
    crate::conformance::helpers::record_completed_attachment_write(
        &store,
        AttachmentIntent {
            attachment_id: attachment.clone(),
            session_id: SessionId::from("committed-owner"),
            canonical_uri: format!("session:committed-owner:sha256:{attachment}"),
            intent_at_epoch_ms: 150,
            owner: None,
        },
    )
    .await;
    assert!(
        !store
            .list_uncommitted(200).await
            .expect("committed ownership remains stamped")
            .iter()
            .any(|entry| entry.session_id == "committed-owner"
                && entry.attachment_id == attachment),
        "a colliding owner or repeated put must not erase another session's commit stamp"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_source_keys_are_idempotent_and_list_ordered(
    store: Arc<dyn RuntimePersistence>,
) {
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
            "different replay payload",
            DeliveryPolicy::EarliestSafeBoundary,
            "source:first",
        ))
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing(
    store: Arc<dyn RuntimePersistence>,
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
    store: Arc<dyn RuntimePersistence>,
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

/// Race two admissions of the same rows from separate tasks; returns both
/// outcomes.
async fn race<T: Send + 'static>(
    left: impl std::future::Future<Output = T> + Send + 'static,
    right: impl std::future::Future<Output = T> + Send + 'static,
) -> (T, T) {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left.await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right.await
    });
    barrier.wait().await;
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a racing task that panics fails the law"
    )]
    let joined = (
        left.await.expect("join the left admission"),
        right.await.expect("join the right admission"),
    );
    joined
}

/// One admission of a contested head: `Some` names the root that took it.
fn admitted_by(
    outcome: Result<Option<lash_core::store::RootAdmission>, StoreError>,
    root: &str,
) -> Option<String> {
    match outcome {
        Ok(Some(_)) => Some(root.to_string()),
        Ok(None) | Err(StoreError::UnfinishedRootConflict { .. }) => None,
        Err(error) => panic!("a contested admission resolves cleanly, got {error:?}"),
    }
}

/// FIG-3927: concurrent admissions bind every row to at most one root. Two
/// roots racing for the same batch head, then for the same input head, and
/// two checkpoint steps racing for the same active-turn input: exactly one
/// takes each row, and the store reads it back bound to the winner.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_admissions_bind_every_row_to_at_most_one_root(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("concurrent-queue-input");
    let batch = store
        .enqueue_queued_work(queued_draft(
            &session_id,
            "single-owner queue batch",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue queue batch for the admission race");
    let fence = seal_drive_fence_for_test(&store, &session_id, "admission-race").await;

    let admit = |root: &'static str, head: lash_core::store::AdmittedHead| {
        let store = Arc::clone(&store);
        let fence = fence.clone();
        async move {
            admitted_by(
                admit_root_for_test(&store, &fence, &TurnId::from(root), head).await,
                root,
            )
        }
    };
    let batch_head = lash_core::store::AdmittedHead::Batch(batch.batch_id.clone());
    let (left, right) = race(
        admit("batch-left", batch_head.clone()),
        admit("batch-right", batch_head),
    )
    .await;
    let winners = [left, right].into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(
        winners.len(),
        1,
        "exactly one root may admit the same batch"
    );
    assert!(
        store
            .list_open_queued_work(&session_id)
            .await
            .expect("list queue after the admission race")
            .is_empty(),
        "the winning admission binds its batch"
    );
    let batch_root = winners[0].clone();
    let recorded = admit_root_for_test(
        &store,
        &fence,
        &TurnId::from(batch_root.as_str()),
        lash_core::store::AdmittedHead::Batch(batch.batch_id.clone()),
    )
    .await
    .expect("read the winner's admission back")
    .expect("the winner holds its admission");
    end_root(&store, &fence, completing_admission(&batch_root, &recorded)).await;

    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session_id,
            "single-owner turn input",
        ))
        .await
        .expect("enqueue turn input for the admission race");
    let input_head = lash_core::store::AdmittedHead::Input(input.input_id.clone());
    let (left, right) = race(
        admit("input-left", input_head.clone()),
        admit("input-right", input_head),
    )
    .await;
    let winners = [left, right].into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(
        winners.len(),
        1,
        "exactly one root may admit the same input"
    );
    assert_eq!(
        store
            .root_of_input(&session_id, &input.input_id)
            .await
            .expect("read the input's root")
            .map(|root| root.to_string()),
        Some(winners[0].clone()),
        "the input reads back bound to the winning root"
    );

    let turn = TurnId::from(winners[0].as_str());
    let active = store
        .enqueue_pending_turn_input(pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            "single-owner active input",
        ))
        .await
        .expect("enqueue an active-turn input for the checkpoint race");
    let checkpoint = |step: &'static str| {
        let store = Arc::clone(&store);
        let fence = fence.clone();
        let turn = turn.clone();
        async move {
            admit_at_checkpoint_for_test(
                &store,
                &fence,
                &turn,
                &turn,
                crate::CheckpointKind::AfterWork,
                step,
                8,
                crate::testing::queued_work_admission_policy(8),
            )
            .await
            .expect("a contested checkpoint admission resolves cleanly")
            .inputs
            .map(|inputs| (step, inputs.input_ids()))
        }
    };
    let (left, right) = race(checkpoint("step-left"), checkpoint("step-right")).await;
    let winners = [left, right].into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(
        winners.len(),
        1,
        "exactly one checkpoint step may admit the same active-turn input"
    );
    assert_eq!(winners[0].1, vec![active.input_id.clone()]);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_cancel_removes_only_open_batches(store: Arc<dyn RuntimePersistence>) {
    let session = SessionId::from("queued-work-cancel");
    let cancellable = store
        .enqueue_queued_work(queued_draft(
            &session,
            "cancel me",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue cancellable batch");
    let cancelled = store
        .cancel_queued_work_batch(&session, &cancellable.batch_id)
        .await
        .expect("cancel an open batch")
        .expect("the open batch is returned");
    assert_eq!(cancelled.batch_id, cancellable.batch_id);
    assert_eq!(queued_batch_text(&cancelled), Some("cancel me"));
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("list after cancellation")
            .is_empty(),
        "cancelled batches must be removed from the durable queue"
    );

    let admitted = store
        .enqueue_queued_work(queued_draft(
            &session,
            "admitted",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue the batch a root admits");
    let fence = seal_drive_fence_for_test(&store, &session, "owner").await;
    let admission = admitted_root(
        &store,
        &fence,
        "cancel-root",
        lash_core::store::AdmittedHead::Batch(admitted.batch_id.clone()),
    )
    .await;
    assert_eq!(admission.batch_ids(), vec![admitted.batch_id.clone()]);
    assert!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open work while admitted")
            .is_empty(),
        "admitted batches must disappear from user-editable queue snapshots"
    );
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("raw durable list while admitted")
            .len(),
        1,
        "admitted batches remain durable until their root settles them"
    );
    assert!(
        store
            .cancel_queued_work_batch(&session, &admitted.batch_id)
            .await
            .expect("cancel an admitted batch")
            .is_none(),
        "admitted batches must not be cancelled"
    );
    end_root(
        &store,
        &fence,
        releasing("cancel-root", [batch_row(&admitted)]),
    )
    .await;
    assert_eq!(
        store
            .list_open_queued_work(&session)
            .await
            .expect("list open work after release")
            .len(),
        1,
        "a released batch becomes user-editable queue work again"
    );
    assert!(
        store
            .cancel_queued_work_batch(&session, &admitted.batch_id)
            .await
            .expect("cancel a released batch")
            .is_some(),
        "a released batch becomes cancellable again"
    );
}

/// The command lane goes first and binds nothing (design §2.7): a turn head
/// behind an open command is not admitted until the command applies. A
/// command enqueued behind the turn head a drive already chose never holds
/// that root's admission back (ADR 0101 §4); it goes first at the next
/// boundary. A turn admission takes only turn work.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_classes_gate_command_and_turn_admissions(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("queued-work-classes");
    let fence = seal_drive_fence_for_test(&store, &session, "turn-owner").await;
    for (case, command_first) in [("command-first", true), ("turn-first", false)] {
        let command_draft = queued_session_command_draft(&session, &format!("{case} refresh"));
        let turn_draft = queued_draft(
            &session,
            &format!("{case} turn"),
            DeliveryPolicy::AfterCurrentTurnCommit,
        );
        let (command, turn) = if command_first {
            let command = store
                .enqueue_queued_work(command_draft)
                .await
                .expect("enqueue command");
            let turn = store
                .enqueue_queued_work(turn_draft)
                .await
                .expect("enqueue turn");
            (command, turn)
        } else {
            let turn = store
                .enqueue_queued_work(turn_draft)
                .await
                .expect("enqueue turn");
            let command = store
                .enqueue_queued_work(command_draft)
                .await
                .expect("enqueue command");
            (command, turn)
        };
        let root = format!("{case}-turn-root");
        let early = admit_root_for_test(
            &store,
            &fence,
            &TurnId::from(root.as_str()),
            lash_core::store::AdmittedHead::Batch(turn.batch_id.clone()),
        )
        .await
        .expect("admission while a command is open");
        if command_first {
            assert!(
                early.is_none(),
                "{case}: a turn head behind an open command waits for it"
            );
        } else {
            assert_eq!(
                early.map(|admission| admission.batch_ids()),
                Some(vec![turn.batch_id.clone()]),
                "{case}: a command enqueued behind the turn head never holds it back"
            );
        }
        let run = store
            .open_session_command_run(&fence)
            .await
            .expect("open the command run");
        assert_eq!(
            run.iter()
                .map(|batch| batch.batch_id.clone())
                .collect::<Vec<_>>(),
            vec![command.batch_id.clone()],
            "{case}: the command lane goes first"
        );
        store
            .commit_runtime_state(applying_commands(
                head_commit(&store, &session).await,
                &fence,
                crate::QueuedWorkCompletion {
                    session_id: session.clone(),
                    batch_ids: run.iter().map(|batch| batch.batch_id.clone()).collect(),
                },
            ))
            .await
            .expect("the command's applying commit settles it");
        let admitted = admitted_root(
            &store,
            &fence,
            &root,
            lash_core::store::AdmittedHead::Batch(turn.batch_id.clone()),
        )
        .await;
        assert_eq!(
            admitted.batch_ids(),
            vec![turn.batch_id.clone()],
            "{case}: a turn admission takes only turn work"
        );
        end_root(&store, &fence, completing_admission(&root, &admitted)).await;
    }
}

/// Queued work waits for the boundary its delivery policy names, and a
/// completion settles only under the live fence of the root holding the rows.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_work_admission_respects_boundaries_and_stale_completion(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("queued-work-boundaries");
    let after_commit = store
        .enqueue_queued_work(queued_draft(
            &session,
            "after current commit",
            DeliveryPolicy::AfterCurrentTurnCommit,
        ))
        .await
        .expect("enqueue after-commit work");
    let earliest = store
        .enqueue_queued_work(queued_draft(
            &session,
            "earliest",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue earliest work");

    let fence = seal_drive_fence_for_test(&store, &session, "owner-a").await;
    let root = TurnId::from("boundary-root");
    let checkpoint = |step: &'static str| {
        let store = Arc::clone(&store);
        let fence = fence.clone();
        let root = root.clone();
        async move {
            admit_at_checkpoint_for_test(
                &store,
                &fence,
                &root,
                &root,
                crate::CheckpointKind::AfterWork,
                step,
                10,
                crate::testing::queued_work_admission_policy(10),
            )
            .await
            .expect("checkpoint admission")
        }
    };
    assert!(
        checkpoint("boundary:step:1").await.is_empty(),
        "after-current-commit work at the queue head must wait for the idle boundary"
    );

    let idle = admitted_root(
        &store,
        &fence,
        "boundary-root",
        lash_core::store::AdmittedHead::Batch(after_commit.batch_id.clone()),
    )
    .await;
    assert_eq!(idle.batch_ids(), vec![after_commit.batch_id.clone()]);

    // With the after-commit head bound to the root, the checkpoint boundary
    // reaches the earliest-safe-boundary batch behind it.
    let at_checkpoint = checkpoint("boundary:step:2").await;
    assert_eq!(
        at_checkpoint
            .queued
            .as_ref()
            .map(|queued| queued.batch_ids())
            .unwrap_or_default(),
        vec![earliest.batch_id.clone()]
    );

    let settlement =
        completing_checkpoint(completing_admission("boundary-root", &idle), &at_checkpoint);
    let mut foreign = settlement.clone();
    foreign.root = TurnId::from("another-root");
    let err = try_end_root(&store, &fence, foreign)
        .await
        .expect_err("a completion keyed by another root must be rejected");
    assert!(
        matches!(err, StoreError::IngressRowNotAdmitted { .. }),
        "a foreign-root completion produced the wrong error: {err:?}"
    );
    let successor = seal_drive_fence_for_test(&store, &session, "owner-b").await;
    let err = try_end_root(&store, &fence, settlement.clone())
        .await
        .expect_err("a completion under a superseded fence must be rejected");
    assert!(
        matches!(err, StoreError::StaleDriveFence { .. }),
        "a stale completion produced the wrong error: {err:?}"
    );
    assert_eq!(
        store
            .list_queued_work(&session)
            .await
            .expect("refused completions preserve queued work")
            .len(),
        2,
        "a refused completion must not delete an admitted batch"
    );
    end_root(&store, &successor, settlement).await;
    assert!(
        store
            .list_queued_work(&session)
            .await
            .expect("the live completion settles both")
            .is_empty()
    );
}
