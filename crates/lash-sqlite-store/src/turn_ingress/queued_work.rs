//! `queued_work_batches` statements only SQLite issues.
//!
//! The admission scan is where the two backends diverge most. PostgreSQL takes
//! `FOR UPDATE` over the candidate rows; SQLite needs no row lock, because the
//! scan already runs inside `BEGIN IMMEDIATE`. The delivery-boundary rule
//! itself is the same rule, spelled twice.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only SQLite issues.
    pub(crate) struct QueuedBatchSqliteStatements @ "queued_work_batch" {
        /// `?9` is allocated from the shared session counter under the write
        /// lock; `?10` is the submission digest. The source key was read in
        /// the same transaction, so its unique constraint is only the
        /// backstop.
        insert_new = "INSERT INTO queued_work_batches (enqueue_seq,
                 batch_id, session_id, source_key, delivery_policy,
                 authority_json, merge_key, enqueued_at_ms, submission_digest, payload_json,
                 trace_cause_json
             )
             VALUES (?8, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?9, ?10, ?11)";

        /// The facts the settlement verdict consults about live batch `?2`
        /// of session `?1`: a tombstone answers nothing, as a missing row
        /// would.
        ///
        /// No lock suffix: the commit already holds the database write lock.
        settlement_facts = "SELECT admitted_run
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2 AND terminal_cause IS NULL";

        /// Batch `?2` of session `?1`, if it is open and no run admitted it.
        ///
        /// Same lock fork as [`settlement_facts`](Self::settlement_facts).
        select_cancelable = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    authority_json, merge_key, enqueued_at_ms, submission_digest,
                    admitted_run, admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_run IS NULL AND terminal_cause IS NULL";

        /// Session `?1`'s admission candidates with no turn in progress, up to
        /// `?2` of them.
        ///
        /// At an idle boundary the head is whatever is open, commands first:
        /// the command lane drains before the turn lane (ADR 0101 §4). The
        /// candidate set is the open command prefix from the head onwards.
        admission_candidates_idle = "WITH queued_work_head_candidate AS (
                 SELECT enqueue_seq AS head_enqueue_seq
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND admitted_run IS NULL AND terminal_cause IS NULL
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    authority_json, merge_key, enqueued_at_ms, submission_digest,
                    admitted_run, admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1
               AND admitted_run IS NULL AND terminal_cause IS NULL
               AND enqueue_seq >= head_enqueue_seq
             ORDER BY enqueue_seq ASC
             LIMIT ?2";

    }
}
