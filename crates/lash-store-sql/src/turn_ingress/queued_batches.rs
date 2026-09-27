//! `queued_work_batches`: one row per batch of work enqueued against a session.

/// The table's unprefixed name.
pub const TABLE: &str = "queued_work_batches";

/// Every column a reader decodes.
///
/// Both backends carried this as a 14-element `QUEUED_WORK_COLUMNS` array that
/// each call site `join(", ")`ed into a `format!`; it is one list now, and the
/// row decoders read by column name so the order is the list's to choose.
pub const COLUMNS: &str = "enqueue_seq, batch_id, session_id, source_key, delivery_policy,
     work_kind, authority_json, merge_key, enqueued_at_ms,
     claim_fencing_token, claim_token, claim_session_lease_generation, claim_id";

/// The columns written after allocation under the session lock.
pub const INSERT_COLUMNS: &str =
    "enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
     authority_json, merge_key, enqueued_at_ms";

/// The columns written by the PostgreSQL insert.
pub const INSERT_COLUMNS_WITH_SEQ: &str = "enqueue_seq, batch_id, session_id, source_key,
     delivery_policy, work_kind, authority_json, merge_key, enqueued_at_ms";

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
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE batch_id = ?1";

        /// The batch session `?1` filed under source key `?2`.
        select_id_by_source_key = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1 AND source_key = ?2";

        /// Every batch of session `?1`, in enqueue order.
        list_by_session = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
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
                    work_kind, authority_json, merge_key, enqueued_at_ms,
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

        /// Session `?1`'s head for generation `?2`, unfiltered by the
        /// delivery boundary: what an empty candidate scan is asked about so
        /// the refusal it reports names the head's own reason.
        select_head_candidate = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key,
                    enqueued_at_ms, claim_fencing_token, claim_token,
                    claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
             ORDER BY enqueue_seq ASC
             LIMIT 1";

        /// Session `?1`'s unclaimed batches for generation `?2` whose
        /// `enqueue_seq` lies between `?3` and `?4`: the span an exact claim
        /// must be contiguous over.
        select_span = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?2)
               AND enqueue_seq BETWEEN ?3 AND ?4
             ORDER BY enqueue_seq ASC";

        /// Claim batch `?2` of session `?1` for claim `?3`, lease token `?4`,
        /// generation `?5`, fencing token `?6`, at `?7`.
        ///
        /// The claim is the batch's admission, so it delivers the batch's
        /// ingress obligation in the same write (ADR 0109 §3).
        ///
        /// The generation predicate stays on the statement as the backstop of
        /// [`queued_work_batch_claimability`](lash_core::store_backend_support::queued_work_batch_claimability):
        /// the verdict decides over the locked row, and a row count other than
        /// one is a disagreement between the two.
        claim = "UPDATE queued_work_batches
             SET claim_id = ?3,
                 claim_token = ?4,
                 claim_fencing_token = ?6,
                 claim_session_lease_generation = ?5,
                 obligation_state = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN 'delivered' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_due_at_ms END,
                 obligation_claim_token = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_claim_token END,
                 obligation_stall_reason = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_stall_reason END,
                 obligation_settled_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN ?7 ELSE obligation_settled_at_ms END
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
        ///
        /// A row handed back to the queue owes its session a drive again: a
        /// delivered ingress obligation is due at once (ADR 0109 §3), and
        /// its next claim asks under a fresh attempt.
        abandon_claim = "UPDATE queued_work_batches
             SET claim_id = ?4,
                 claim_token = ?5,
                 claim_session_lease_generation = 0,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
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

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct BatchRootVerbStatements @ "queued_work_batch" {
        release_batches = "UPDATE queued_work_batches SET
            claim_id = NULL,
            claim_token = NULL, claim_session_lease_generation = 0
            WHERE session_id = ?1 AND claim_token IS NOT NULL";
        delete_batch = "DELETE FROM queued_work_batches WHERE session_id = ?1 AND batch_id = ?2";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str =
    "obligation_id, obligation_attempts, session_id, batch_id";

/// A stalled obligation as an operator lists it: [`OBLIGATION_CLAIM_COLUMNS`]
/// with the stall's reason, last error and instant before the key.
pub const OBLIGATION_STALLED_COLUMNS: &str = "obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id, batch_id";

/// A due obligation's instant and id, read without claiming it: what the
/// ingress ledger merges across its two tables before it claims either
/// (ADR 0109 §3).
///
/// Narrow because the merge only orders by due instant and names which table
/// to claim from; the claim itself reads [`OBLIGATION_CLAIM_COLUMNS`].
pub const OBLIGATION_DUE_COLUMNS: &str = "obligation_due_at_ms, obligation_id";

crate::statements! {
    /// `queued_work_batches` obligation statements (ADR 0109): an admitted
    /// batch owes its session a drive. Both backends issue them verbatim;
    /// every settling write compares the state and, while claimed, the claim
    /// token.
    pub struct QueuedBatchObligationStatements @ "queued_work_batch" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at `?4`, if
        /// it owes nothing.
        obligation_arm = "UPDATE queued_work_batches
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE session_id = ?1 AND batch_id = ?2 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM queued_work_batches
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// The due instant and id of at most `?2` obligations due at `?1`,
        /// oldest due first: what the ingress ledger merges across its two
        /// tables before it claims either.
        obligation_peek_due = "SELECT obligation_due_at_ms, obligation_id FROM queued_work_batches
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE queued_work_batches
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, session_id, batch_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE queued_work_batches
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, session_id, batch_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE queued_work_batches
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE queued_work_batches
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE queued_work_batches
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE queued_work_batches
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id, batch_id
             FROM queued_work_batches
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM queued_work_batches WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state.
        obligation_select_state = "SELECT obligation_state FROM queued_work_batches WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for QueuedBatchObligationStatements {
    fn obligation_sql(&self) -> crate::obligation::ObligationSql<'_> {
        crate::obligation::ObligationSql {
            key_columns: 2,
            arm: &self.obligation_arm,
            select_due: &self.obligation_select_due,
            claim_due_row: &self.obligation_claim_due_row,
            claim: &self.obligation_claim,
            settle_delivered: &self.obligation_settle_delivered,
            settle_retry: &self.obligation_settle_retry,
            settle_stall: &self.obligation_settle_stall,
            rearm: &self.obligation_rearm,
            select_stalled: &self.obligation_select_stalled,
            count_stalled: &self.obligation_count_stalled,
            select_state: &self.obligation_select_state,
        }
    }
}
