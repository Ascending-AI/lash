//! The session ingress's integrity laws (ADR 0101 §5.1, §5.2, §8): a
//! submitted delivery is never rewritten, a turn address the session cannot
//! name is refused before anything is allocated, every kind answers a
//! resubmission by its digest, every kind leaves a tombstone with a closed
//! cause, and `authority` and `merge_key` are per-item data rather than
//! composition gates.

use super::*;
use crate::conformance::admission_support::*;
use lash_core::store::{IngressTerminal, IngressTerminalCause};
use pretty_assertions::assert_eq;

/// A `RefreshToolCatalog` command for `reason` filed under `source_key`.
fn keyed_command(session: &SessionId, source_key: &str, reason: &str) -> QueuedWorkBatchDraft {
    queued_session_command_draft(session, reason).with_source_key(source_key)
}

/// The terminal of a batch an identical resubmission answers: it must be
/// answered as the existing batch, never enqueued again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn resubmitted_terminal(
    store: &Arc<dyn RuntimeStore>,
    draft: QueuedWorkBatchDraft,
    original: &QueuedWorkBatch,
) -> Option<IngressTerminal> {
    let outcome = store
        .enqueue_queued_work_with_outcome(draft)
        .await
        .expect("an identical resubmission is answered");
    let crate::QueuedWorkEnqueueOutcome::Existing(existing) = outcome else {
        panic!("an identical resubmission must answer the stored batch, not enqueue another");
    };
    assert_eq!(existing.batch_id, original.batch_id);
    assert_eq!(existing.enqueue_seq, original.enqueue_seq);
    assert_eq!(existing.submission_digest, original.submission_digest);
    assert!(
        serde_json::to_value(&existing.payload).expect("encode payload")
            == serde_json::to_value(&original.payload).expect("encode original payload"),
        "the tombstone keeps its submission"
    );
    existing.terminal
}

/// ADR 0101 §8: every kind records an immutable submission digest. The same
/// source key and digest answers the stored batch; a changed digest is the
/// typed `QueuedWorkSourceKeyConflict`, and nothing is stored or adopted. A
/// wake's identity is its process fact, so a redelivery under different
/// host-configured delivery policy or merge key is the same submission.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_changed_resubmission_is_a_typed_conflict_for_every_kind(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-content-conflict");
    let conflicting = |draft: QueuedWorkBatchDraft, original: QueuedWorkBatch| {
        let store = Arc::clone(&store);
        async move {
            let session = draft.session_id.clone();
            let before = store
                .list_queued_work(&session)
                .await
                .expect("list queued work");
            let error = store
                .enqueue_queued_work(draft)
                .await
                .expect_err("changed content under a filed source key is refused");
            assert!(
                matches!(
                    &error,
                    StoreError::QueuedWorkSourceKeyConflict { existing_batch_id, .. }
                        if *existing_batch_id == original.batch_id
                ),
                "a changed resubmission is a typed content conflict: {error:?}"
            );
            let after = store
                .list_queued_work(&session)
                .await
                .expect("list queued work");
            assert_eq!(
                after
                    .iter()
                    .map(|batch| (batch.batch_id.clone(), batch.submission_digest.clone()))
                    .collect::<Vec<_>>(),
                before
                    .iter()
                    .map(|batch| (batch.batch_id.clone(), batch.submission_digest.clone()))
                    .collect::<Vec<_>>(),
                "a refused resubmission stores nothing"
            );
        }
    };

    let command = store
        .enqueue_queued_work(keyed_command(&session, "conflict-command", "original"))
        .await
        .expect("enqueue the command");
    assert_eq!(
        resubmitted_terminal(
            &store,
            keyed_command(&session, "conflict-command", "original"),
            &command
        )
        .await,
        None
    );
    conflicting(
        keyed_command(&session, "conflict-command", "changed"),
        command.clone(),
    )
    .await;
    conflicting(
        keyed_command(&session, "conflict-command", "original")
            .with_authority(crate::QueuedWorkAuthority::new("someone else")),
        command,
    )
    .await;
}

/// ADR 0101 §8, as FIG-4202 found missing: a session command resubmitted
/// under its source key after the command lane applied it answers the
/// applied tombstone. It is not a new command, and the command lane has
/// nothing more to apply.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_settled_command_resubmitted_under_its_key_is_not_a_new_command(
    store: Arc<dyn RuntimeStore>,
) {
    let session = SessionId::from("ingress-command-resubmission");
    let command = store
        .enqueue_queued_work(keyed_command(&session, "host-command-1", "refresh"))
        .await
        .expect("enqueue the command");
    store
        .open_session_command_run(&session)
        .await
        .expect("open the command run");
    store
        .commit_runtime_state(applying_commands(
            head_commit(&store, &session).await,
            crate::QueuedWorkCompletion {
                session_id: session.clone(),
                batch_ids: vec![command.batch_id.clone()],
            },
        ))
        .await
        .expect("the applying commit settles the command");
    let terminal = resubmitted_terminal(
        &store,
        keyed_command(&session, "host-command-1", "refresh"),
        &command,
    )
    .await;
    assert_eq!(
        terminal.map(|terminal| terminal.cause),
        Some(IngressTerminalCause::Applied)
    );
    assert!(
        store
            .open_session_command_run(&session)
            .await
            .expect("open the command run")
            .is_empty(),
        "the resubmission queued no second command"
    );
}

/// A recorded queued-work admission has one payload, so replay cannot hide
/// an unfenced second wake in the same batch.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn a_recorded_queued_batch_refuses_multiple_payloads(store: Arc<dyn RuntimeStore>) {
    let session = SessionId::from("ingress-one-payload");
    let batch = store
        .enqueue_queued_work(keyed_command(&session, "single-command", "one command"))
        .await
        .expect("enqueue one wake");
    let recorded = serde_json::to_value(&batch).expect("record the batch");
    let restored: QueuedWorkBatch =
        serde_json::from_value(recorded.clone()).expect("restore one payload");
    assert_eq!(restored.batch_id, batch.batch_id);
    let payload = recorded["payload"].clone();
    let mut multiple = recorded.clone();
    multiple["payload"] = serde_json::json!([payload.clone(), payload.clone()]);
    assert!(
        serde_json::from_value::<QueuedWorkBatch>(multiple).is_err(),
        "a recorded admission must refuse multiple payloads"
    );
    let mut legacy = recorded;
    legacy
        .as_object_mut()
        .expect("batch is an object")
        .remove("payload");
    legacy["items"] = serde_json::json!([
        {"item_id": "first", "payload": payload.clone()},
        {"item_id": "second", "payload": payload}
    ]);
    assert!(
        serde_json::from_value::<QueuedWorkBatch>(legacy).is_err(),
        "the retired item array cannot cross the journal boundary"
    );
}
