//! `queued_work_batches`: one row per batch of work enqueued against a session.

/// The table's unprefixed name.
pub const TABLE: &str = "queued_work_batches";

/// Every column a reader decodes.
///
/// Both backends carried this as a 14-element `QUEUED_WORK_COLUMNS` array that
/// each call site `join(", ")`ed into a `format!`; it is one list now, and the
/// row decoders read by column name so the order is the list's to choose.
pub const COLUMNS: &str = "enqueue_seq, batch_id, session_id, source_key, delivery_policy,
     work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by";

/// The columns written after allocation under the session lock.
pub const INSERT_COLUMNS: &str =
    "enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
     authority_json, merge_key, enqueued_at_ms";

/// The facts the settlement verdict
/// [`require_admitted_to_root`](lash_core::store_backend_support::require_admitted_to_root)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled batch of every commit and
/// no part of the settlement decision looks at `authority_json`, which is an
/// unbounded caller-supplied envelope.
pub const SETTLEMENT_COLUMNS: &str = "admitted_root";

crate::statements! {
    /// `queued_work_batches` statements both backends issue verbatim.
    pub struct QueuedBatchStatements @ "queued_work_batch" {
        select_by_id = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE batch_id = ?1";

        /// The batch session `?1` filed under source key `?2`.
        select_id_by_source_key = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1 AND source_key = ?2";

        /// Every batch of session `?1`, in enqueue order.
        list_by_session = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE session_id = ?1
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s open batches: every batch no root has admitted, in
        /// enqueue order. Session commands are never admitted, so they are
        /// always open.
        list_open = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_root IS NULL
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s batches root `?2` bound under step `?3`, in
        /// `enqueue_seq` order: what a re-executed admission step reads back
        /// instead of choosing again (FIG-3927).
        select_admitted_by_step = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key,
                    enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_root = ?2 AND admitted_by = ?3
             ORDER BY enqueue_seq ASC";

        /// Admit open batch `?2` of session `?1` to root `?3` by step `?4`,
        /// at `?5`.
        ///
        /// The admission is the batch's delivery, so it delivers the batch's
        /// ingress obligation in the same write (ADR 0109 §3). The open
        /// predicate is the write's backstop: the composition was read in the
        /// same transaction, so a row count other than one is a disagreement
        /// between the two.
        admit = "UPDATE queued_work_batches
             SET admitted_root = ?3,
                 admitted_by = ?4,
                 obligation_state = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN 'delivered' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_due_at_ms END,
                 obligation_claim_token = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_claim_token END,
                 obligation_stall_reason = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_stall_reason END,
                 obligation_settled_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN ?5 ELSE obligation_settled_at_ms END
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL";

        /// Deliver the ingress obligation of open session command `?2` of
        /// session `?1` at `?3`: the command lane takes no admission, so the
        /// drive that reads a command run acknowledges it here, in one fenced
        /// write, before it applies the run (ADR 0109 §3).
        deliver_open_command = "UPDATE queued_work_batches
             SET obligation_state = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN 'delivered' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_due_at_ms END,
                 obligation_claim_token = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_claim_token END,
                 obligation_stall_reason = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_stall_reason END,
                 obligation_settled_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN ?3 ELSE obligation_settled_at_ms END
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL";

        /// The first payload of batch `?2` of session `?1`, if root `?3`
        /// still holds it: the wake identity a settled batch contributes to
        /// its redelivery fence.
        ///
        /// The wake-source key travels in the plan's covered-item list, so the
        /// payload is the only fact still on the row. Both backends decode it
        /// the same way; the wake batch carries exactly one wake item
        /// (`validate_process_wake_source`), so the head payload is the batch's
        /// whole wake contribution.
        select_admitted_batch_head_payload = "SELECT item.payload_json
             FROM queued_work_batches AS batch
             JOIN queued_work_items AS item ON item.batch_id = batch.batch_id
             WHERE batch.session_id = ?1
               AND batch.batch_id = ?2
               AND batch.admitted_root = ?3
             ORDER BY item.item_index ASC
             LIMIT 1";

        /// Settle batch `?2` of session `?1` under root `?3`, which must hold
        /// it, by removing it. A settled batch has no resting state: its items
        /// go with it through the foreign key's cascade.
        settle_admitted = "DELETE FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root = ?3";

        /// Settle open session command `?2` of session `?1` by removing it:
        /// the command lane's settlement, in the commit that applied it
        /// (FIG-3927). A command withdrawn since the drive read it matches no
        /// row, and the commit is refused.
        settle_command = "DELETE FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL";

        /// Hand batch `?2` of session `?1` back open at its own position,
        /// under root `?3`, which must hold it.
        ///
        /// A row handed back to the queue owes its session a drive again: a
        /// delivered ingress obligation is due at once (ADR 0109 §3). A
        /// released wake keeps its redelivery floor: the fence is raised only
        /// by a wake's terminal transition.
        release_admitted = "UPDATE queued_work_batches
             SET admitted_root = NULL,
                 admitted_by = NULL,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND batch_id = ?2 AND admitted_root = ?3";

        /// [`release_admitted`](Self::release_admitted) over every batch root
        /// `?2` of session `?1` still holds: the root's terminal write, after
        /// the settlements its commit named (FIG-3927).
        release_root = "UPDATE queued_work_batches
             SET admitted_root = NULL,
                 admitted_by = NULL,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND admitted_root = ?2";

        delete_by_session = "DELETE FROM queued_work_batches WHERE session_id = ?1";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct BatchRootVerbStatements @ "queued_work_batch" {
        delete_batch = "DELETE FROM queued_work_batches WHERE session_id = ?1 AND batch_id = ?2";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str =
    "obligation_id, obligation_attempts, session_id, batch_id";

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

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM queued_work_batches WHERE obligation_id = ?1";
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
            select_standing: &self.obligation_select_standing,
        }
    }
}
