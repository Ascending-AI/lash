//! `queued_work_batches` statements only PostgreSQL issues.
//!
//! The admission scan is where the two backends diverge most. PostgreSQL
//! takes `FOR UPDATE` over the candidate rows, so a concurrent host
//! withdrawal either precedes the admission or waits for it; SQLite needs no
//! row lock under `BEGIN IMMEDIATE`.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only PostgreSQL issues.
    pub(crate) struct QueuedBatchPostgresStatements @ "queued_work_batch" {
        /// At most `?2` ingress obligations due at `?1`, oldest due first,
        /// each row locked for the caller's obligation claim and skipped by
        /// every concurrent claimant: two deployments' relays take disjoint pages
        /// (ADR 0109 §1.7).
        obligation_select_due_locking = "SELECT obligation_id FROM queued_work_batches
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";

        insert_new = "INSERT INTO queued_work_batches (
                 enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
                 authority_json, merge_key, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (session_id, source_key) DO NOTHING
             RETURNING batch_id";

        /// The facts the settlement verdict consults about batch `?2` of
        /// session `?1`, locked for the caller's transaction.
        settlement_facts = "SELECT admitted_root
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2 LIMIT 1 FOR UPDATE";

        /// Batch `?2` of session `?1`, if it is open, locked for the caller's
        /// transaction.
        select_cancelable = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL
             FOR UPDATE";

        /// Withdraw batch `?1`.
        ///
        /// Keyed by id alone because the row lock
        /// [`select_cancelable`](Self::select_cancelable) took is what holds the
        /// openness decision; SQLite has no row lock, so it repeats the
        /// predicate on its delete.
        delete_cancelled = "DELETE FROM queued_work_batches WHERE batch_id = ?1";

        /// Session `?1`'s admission candidates with no turn in progress, up to
        /// `?2` of them.
        ///
        /// At an idle boundary the head is whatever is open, commands first:
        /// the command lane drains before the turn lane (ADR 0101 §4). The
        /// candidate set is the run of the head's own kind from the head
        /// onwards.
        admission_candidates_idle = "WITH queued_work_head_candidate AS (
                 SELECT enqueue_seq AS head_enqueue_seq, work_kind AS head_work_kind
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND admitted_root IS NULL
                 ORDER BY CASE WHEN work_kind = 'control' THEN 0 ELSE 1 END, enqueue_seq ASC
                 LIMIT 1
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1
               AND admitted_root IS NULL
               AND enqueue_seq >= head_enqueue_seq
               AND work_kind = head_work_kind
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE OF queued_work_batches";

        /// Session `?1`'s open queued turn work a root's admission composes
        /// from at an idle boundary, up to `?2` of them (ADR 0101 §4, §5).
        ///
        /// The admission chose the turn lane at a boundary whose command
        /// lane was empty, so a command enqueued since holds back only the
        /// rows after it: the run starts at the earliest open turn work and
        /// ends at the earliest open command, exactly as the next-turn input
        /// scan does.
        admission_candidates_turn_lane = "WITH queued_work_head_candidate AS (
                 SELECT enqueue_seq AS head_enqueue_seq
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn' AND admitted_root IS NULL
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1 AND work_kind = 'turn'
               AND admitted_root IS NULL
               AND enqueue_seq >= head_enqueue_seq
               AND NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS commands
                    WHERE commands.session_id = ?1 AND commands.work_kind = 'control'
                      AND commands.enqueue_seq < queued_work_batches.enqueue_seq
               )
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE OF queued_work_batches";

        /// [`admission_candidates_idle`](Self::admission_candidates_idle) at
        /// a turn checkpoint, where only work whose delivery policy admits the
        /// earliest safe boundary may start: an open head that must wait for
        /// the current turn's commit blocks everything behind it.
        admission_candidates_boundary = "WITH queued_work_head_candidate AS (
                 SELECT enqueue_seq AS head_enqueue_seq,
                        delivery_policy AS head_delivery_policy
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn' AND admitted_root IS NULL
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1 AND work_kind = 'turn'
               AND admitted_root IS NULL
               AND head_delivery_policy = 'earliest_safe_boundary'
               AND enqueue_seq >= head_enqueue_seq
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE OF queued_work_batches SKIP LOCKED";
    }
}

lash_store_sql::statements! {
    /// `queued_work_items` statements only PostgreSQL issues.
    pub(crate) struct QueuedItemPostgresStatements @ "queued_work_item" {
        /// SQLite gets this from the foreign key's `ON DELETE CASCADE`; this
        /// schema's constraint is not declared cascading, so the sweep names
        /// the rows itself.
        delete_by_session = "DELETE FROM queued_work_items
             WHERE batch_id IN (
                 SELECT batch_id FROM queued_work_batches WHERE session_id = ?1
             )";
    }
}
