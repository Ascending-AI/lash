//! `queued_work_batches`: one row per batch of work enqueued against a session.

/// The table's unprefixed name.
pub const TABLE: &str = "queued_work_batches";

/// Every column a reader decodes.
///
/// Both backends carried this as a 14-element `QUEUED_WORK_COLUMNS` array that
/// each call site `join(", ")`ed into a `format!`; it is one list now, and the
/// row decoders read by column name so the order is the list's to choose.
pub const COLUMNS: &str = "enqueue_seq, batch_id, session_id, source_key, delivery_policy,
     work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
     claim_fencing_token, claim_token, claim_session_lease_generation, claim_id";

/// The columns an enqueue writes on SQLite, where `enqueue_seq` is the table's
/// `INTEGER PRIMARY KEY AUTOINCREMENT` and is never bound.
pub const INSERT_COLUMNS: &str = "batch_id, session_id, source_key, delivery_policy, work_kind,
     authority_json, merge_key, available_at_ms, enqueued_at_ms";

/// The columns an enqueue writes on PostgreSQL, which draws `enqueue_seq` from
/// the column's sequence before the insert so the conflict path can report the
/// value it tried to write.
pub const INSERT_COLUMNS_WITH_SEQ: &str = "enqueue_seq, batch_id, session_id, source_key,
     delivery_policy, work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms";

/// The facts the settlement verdict
/// [`require_settleable_queued_work`](lash_core::store_backend_support::require_settleable_queued_work)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled batch of every commit and
/// no part of the settlement decision looks at `authority_json`, which is an
/// unbounded caller-supplied envelope.
pub const SETTLEMENT_COLUMNS: &str = "claim_id, claim_token, claim_session_lease_generation";

/// The four facts the delivery-boundary rule needs about the queue's head.
///
/// Narrow because the head candidate is a *decision input*, not a row: the
/// boundary rule asks only where the head sits, what its delivery policy is and
/// whether it is already claimed. Reading whole rows to answer that would carry
/// every batch's `authority_json` through the claim path's hottest query.
pub const HEAD_CANDIDATE_COLUMNS: &str = "enqueue_seq AS head_enqueue_seq,
     batch_id AS head_batch_id,
     delivery_policy AS head_delivery_policy,
     claim_id AS head_claim_id";

/// [`HEAD_CANDIDATE_COLUMNS`], qualified for the boundary form's self-join
/// against the unfiltered head.
pub const QUALIFIED_HEAD_CANDIDATE_COLUMNS: &str = "candidate.enqueue_seq AS head_enqueue_seq,
     candidate.batch_id AS head_batch_id,
     candidate.delivery_policy AS head_delivery_policy,
     candidate.claim_id AS head_claim_id";

/// The ordering key the pending-work comparison reads. Narrow because the
/// comparison is between this pair and the pending inputs' identical pair;
/// nothing decodes a row.
pub const ORDERING_COLUMNS: &str = "enqueued_at_ms, enqueue_seq";

crate::statements! {
    /// `queued_work_batches` statements both backends issue verbatim.
    pub struct QueuedBatchStatements @ "queued_work_batch" {
        select_by_id = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE batch_id = ?1";

        /// The batch session `?1` filed under source key `?2`.
        select_id_by_source_key = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1 AND source_key = ?2";

        /// Every batch of session `?1`, in enqueue order.
        list_by_session = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s batches that no live claim holds at `?2`.
        ///
        /// A claim is live only while the session-execution lease generation it
        /// pins still holds the lease, so this is a join against the lease row
        /// rather than a `claim_token IS NULL` test (ADR 0029).
        list_unclaimed = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM session_execution_leases sel
                    WHERE sel.session_id = ?1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > ?2
                      AND sel.lease_fencing_token
                          = queued_work_batches.claim_session_lease_generation
               ))
             ORDER BY enqueue_seq ASC";

        /// Claim batch `?2` of session `?1` for claim `?3`, lease token `?4`,
        /// generation `?5`, fencing token `?6`.
        ///
        /// The generation predicate stays on the statement as the backstop of
        /// [`queued_work_batch_claimability`](lash_core::store_backend_support::queued_work_batch_claimability):
        /// the verdict decides over the locked row, and a row count other than
        /// one is a disagreement between the two.
        claim = "UPDATE queued_work_batches
             SET claim_id = ?3,
                 claim_token = ?4,
                 claim_fencing_token = ?6,
                 claim_session_lease_generation = ?5
             WHERE session_id = ?1
               AND batch_id = ?2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?5
               )";

        /// The first payload of batch `?2` of session `?1`, if claim `?3`/`?4`
        /// still holds it: the wake identity a settled batch contributes to its
        /// redelivery fence.
        ///
        /// The wake-source key travels in the plan's covered-item list, so the
        /// payload is the only fact still on the row. Both backends decode it
        /// the same way; the wake batch carries exactly one wake item
        /// (`validate_process_wake_source`), so the head payload is the batch's
        /// whole wake contribution.
        select_claimed_batch_head_payload = "SELECT item.payload_json
             FROM queued_work_batches AS batch
             JOIN queued_work_items AS item ON item.batch_id = batch.batch_id
             WHERE batch.session_id = ?1
               AND batch.batch_id = ?2
               AND batch.claim_id = ?3
               AND batch.claim_token = ?4
             ORDER BY item.item_index ASC
             LIMIT 1";

        /// Give up claim `?2`/`?3` on session `?1`, restoring the interrupted
        /// predecessor identity `?4`/`?5` the claim displaced.
        abandon_claim = "UPDATE queued_work_batches
             SET claim_id = ?4,
                 claim_token = ?5,
                 claim_session_lease_generation = 0
             WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3";

        /// Settle batch `?2` of session `?1` under claim `?3`/`?4` by removing
        /// it. A settled batch has no resting state: its items go with it
        /// through the foreign key's cascade.
        settle_claimed = "DELETE FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND claim_id = ?3
               AND claim_token = ?4";

        delete_by_session = "DELETE FROM queued_work_batches WHERE session_id = ?1";
    }
}
