use super::*;
use pretty_assertions::assert_eq;

/// Delete, then enqueue, must never hand out a sequence the deleted row
/// already wore (FIG-3632).
///
/// `enqueue_seq` is each ingress family's durable identity and order, and
/// claim ids are derived from it, so a reused sequence aliases a deleted or
/// settled item's claim evidence, dedup keys and observation cursors. The law
/// runs the delete/enqueue cycle twenty times per family: deleting the newest
/// row and enqueuing again is the exact shape under which a store that
/// recycles its maximum sequence repeats an id on every round.
const IDENTITY_REUSE_ROUNDS: usize = 20;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn delete_then_enqueue_never_reuses_ingress_sequences(
    store: Arc<dyn RuntimePersistence>,
) {
    let session = SessionId::from("root");
    let mut last_batch_seq = 0_u64;
    let mut last_input_seq = 0_u64;
    for round in 0..IDENTITY_REUSE_ROUNDS {
        let batch = store
            .enqueue_queued_work(queued_draft(
                &session,
                &format!("queue round {round}"),
                DeliveryPolicy::EarliestSafeBoundary,
            ))
            .await
            .expect("enqueue queued batch");
        assert!(
            batch.enqueue_seq > last_batch_seq,
            "queued_work_batches reissued enqueue_seq {} in round {round} after its row was deleted",
            batch.enqueue_seq
        );
        last_batch_seq = batch.enqueue_seq;
        let cancelled = store
            .cancel_queued_work_batch(&session, &batch.batch_id)
            .await
            .expect("cancel queued batch")
            .expect("unclaimed batch is cancelled");
        assert_eq!(cancelled.batch_id, batch.batch_id);
        assert!(
            store
                .list_queued_work(&session)
                .await
                .expect("list queued work after cancel")
                .is_empty(),
            "the cancelled row must be physically deleted before the next enqueue"
        );

        let input = store
            .enqueue_pending_turn_input(pending_next_turn_input_draft(
                &session,
                &format!("input round {round}"),
            ))
            .await
            .expect("enqueue pending turn input");
        assert!(
            input.enqueue_seq > last_input_seq,
            "pending_turn_inputs reissued enqueue_seq {} in round {round} after its tombstone was deleted",
            input.enqueue_seq
        );
        last_input_seq = input.enqueue_seq;
        expect_cancelled_pending_input(
            store
                .cancel_pending_turn_input(&session, &input.input_id)
                .await
                .expect("cancel pending turn input"),
            &input.input_id,
        );
        let vacuum = store.vacuum().await.expect("vacuum tombstones");
        assert!(
            vacuum.removed_pending_turn_input_tombstone_count >= 1,
            "the cancelled tombstone must be physically deleted before the next enqueue"
        );
        assert!(
            store
                .list_pending_turn_inputs(&session)
                .await
                .expect("list pending inputs after vacuum")
                .is_empty()
        );
    }
}
