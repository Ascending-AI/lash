//! `queued_work_batches` statements only SQLite issues.
//!
//! The admission scan is where the two backends diverge most. PostgreSQL takes
//! `FOR UPDATE` over the candidate rows; SQLite needs no row lock, because the
//! scan already runs inside `BEGIN IMMEDIATE`. The delivery-boundary rule
//! itself is the same rule, spelled twice.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only SQLite issues.
    pub(crate) struct QueuedBatchSqliteStatements @ "queued_work_batch" {
        /// `?9` is allocated from the shared session counter under the write lock.
        insert_new = "INSERT INTO queued_work_batches (enqueue_seq,
                 batch_id, session_id, source_key, delivery_policy, work_kind,
                 authority_json, merge_key, enqueued_at_ms
             )
             VALUES (?9, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (session_id, source_key) DO NOTHING";

        /// The facts the settlement verdict consults about batch `?2` of
        /// session `?1`.
        ///
        /// No lock suffix: the commit already holds the database write lock.
        settlement_facts = "SELECT admitted_root
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2";

        /// Batch `?2` of session `?1`, if it is open.
        ///
        /// Same lock fork as [`settlement_facts`](Self::settlement_facts).
        select_cancelable = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, admitted_root, admitted_by
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL";

        /// Withdraw batch `?2` of session `?1`, if it is open.
        ///
        /// The open predicate is repeated on the delete rather than inherited
        /// from the read above, because SQLite has no row lock to carry it:
        /// the write lock makes the pair atomic, and the predicate makes the
        /// delete truthful on its own. PostgreSQL's read takes `FOR UPDATE`,
        /// so its delete is keyed by id alone.
        delete_cancelled = "DELETE FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root IS NULL";

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
             LIMIT ?2";

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
             LIMIT ?2";

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
             LIMIT ?2";
    }
}

lash_store_sql::statements! {
    /// `queued_work_items` statements only SQLite issues.
    pub(crate) struct QueuedItemSqliteStatements @ "queued_work_item" {
        /// The payloads of every batch id in the JSON array `?1`, keyed by
        /// batch and in item order.
        ///
        /// One page for a whole admission rather than one query per batch:
        /// the admission hydrates a run of batches at once, and under
        /// SQLite's write lock the run cannot change between them anyway.
        /// PostgreSQL hydrates per batch inside a `REPEATABLE READ` snapshot
        /// instead, so it has no counterpart. The list bind is a JSON array
        /// unpacked with `json_each`, which is how this crate binds every
        /// list.
        list_by_batches = "SELECT batch_id, item_id, payload_json
             FROM queued_work_items
             WHERE batch_id IN (SELECT value FROM json_each(?1))
             ORDER BY batch_id ASC, item_index ASC";
    }
}
