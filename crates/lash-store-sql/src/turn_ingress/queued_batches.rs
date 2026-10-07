//! `queued_work_batches`: one row per batch of work enqueued against a session.

/// The table's unprefixed name.
pub const TABLE: &str = "queued_work_batches";

/// Every column a reader decodes.
///
/// Both backends carried this as a 14-element `QUEUED_WORK_COLUMNS` array that
/// each call site `join(", ")`ed into a `format!`; it is one list now, and the
/// row decoders read by column name so the order is the list's to choose.
pub const COLUMNS: &str = "enqueue_seq, batch_id, session_id, source_key, delivery_policy,
     work_kind, authority_json, merge_key, enqueued_at_ms, submission_digest, admitted_run,
     admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json";

/// The columns written after allocation under the session lock.
pub const INSERT_COLUMNS: &str =
    "enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
     authority_json, merge_key, enqueued_at_ms, submission_digest, payload_json, trace_cause_json";

/// The facts the settlement verdict
/// [`require_admitted_to_run`](lash_core::store_backend_support::require_admitted_to_run)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled batch of every commit and
/// no part of the settlement decision looks at `authority_json`, which is an
/// unbounded caller-supplied envelope.
pub const SETTLEMENT_COLUMNS: &str = "admitted_run";

crate::statements! {
    /// `queued_work_batches` statements both backends issue verbatim.
    pub struct QueuedBatchStatements @ "queued_work_batch" {
        select_by_id = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, submission_digest,
                    admitted_run, admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             WHERE batch_id = ?1";

        /// The source key batch `?2` of session `?1` was accepted under: the
        /// turn a run that admits it executes (FIG-3946).
        select_source_key_by_id = "SELECT source_key FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2";

        /// The applying commit's original receipt, reached through the command tombstone.
        select_command_completion = "SELECT receipt.turn_id, receipt.result_json
             FROM queued_work_batches AS batch
             JOIN runtime_turn_commits AS receipt
               ON receipt.session_id = batch.session_id
              AND receipt.turn_id = batch.settled_operation_key
             WHERE batch.session_id = ?1 AND batch.batch_id = ?2";

        /// The id and admission-time submission digest of the batch session
        /// `?1` filed under source key `?2`, open or a tombstone: what a
        /// resubmission's verdict compares (ADR 0101 §8).
        select_id_by_source_key = "SELECT batch_id, submission_digest FROM queued_work_batches
             WHERE session_id = ?1 AND source_key = ?2";

        /// Every live batch of session `?1`, open or admitted, in enqueue
        /// order: tombstones are not queued work.
        list_by_session = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, submission_digest,
                    admitted_run, admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             WHERE session_id = ?1 AND terminal_cause IS NULL
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s open batches: every live batch no run has
        /// admitted, in enqueue order. Session commands are never admitted,
        /// so they are open until their tombstone.
        list_open = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms, submission_digest,
                    admitted_run, admitted_by, terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run IS NULL AND terminal_cause IS NULL
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s batches run `?2` bound under step `?3`, in
        /// `enqueue_seq` order: what a re-executed admission step reads back
        /// instead of choosing again (FIG-3927).
        select_admitted_by_step = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key,
                    enqueued_at_ms, submission_digest, admitted_run, admitted_by,
                    terminal_cause, terminal_at_ms, payload_json, trace_cause_json
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run = ?2 AND admitted_by = ?3
             ORDER BY enqueue_seq ASC";

        /// Admit open batch `?2` of session `?1` to run `?3` by step `?4`,
        /// at `?5`. The open predicate is the write's backstop: the
        /// composition was read in the same transaction, so a row count other
        /// than one is a disagreement between the two.
        admit = "UPDATE queued_work_batches
             SET admitted_run = ?3,
                 admitted_by = ?4
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_run IS NULL
               AND terminal_cause IS NULL";

        /// The sole payload of batch `?2` of session `?1`, if run `?3` holds it.
        select_admitted_batch_payload = "SELECT payload_json FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2 AND admitted_run = ?3";

        /// Settle batch `?2` of session `?1` under run `?3`, which must hold
        /// it, into its tombstone with cause `?4` at `?5` (ADR 0101 §8): the
        /// binding goes and the submission stays with its payload
        /// until host vacuum.
        settle_admitted = "UPDATE queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL,
                 terminal_cause = ?4,
                 terminal_at_ms = ?5
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_run = ?3";

        /// Settle open session command `?2` of session `?1` into its
        /// tombstone at `?3`, with cause `?4` and applying operation `?5`, in the
        /// commit that applied it (FIG-3927). A command withdrawn since the
        /// shift read it matches no row, and the commit is refused.
        settle_command = "UPDATE queued_work_batches
             SET terminal_cause = ?4,
                 terminal_at_ms = ?3,
                 settled_operation_key = ?5
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_run IS NULL
               AND terminal_cause IS NULL";

        /// Withdraw open batch `?2` of session `?1` into its `cancelled`
        /// tombstone at `?3`: the host's withdrawal (ADR 0101 §8, §10). Only
        /// an open batch is withdrawn; a batch a run admitted is that run's
        /// to settle or release.
        withdraw_open = "UPDATE queued_work_batches
             SET terminal_cause = 'cancelled',
                 terminal_at_ms = ?3
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_run IS NULL
               AND terminal_cause IS NULL";

        /// Cancel batch `?2` of session `?1`, held by a run that ends
        /// unanswered, into its `cancelled` tombstone at `?3`, letting go of
        /// its admission (ADR 0101 §8).
        cancel_batch = "UPDATE queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL,
                 terminal_cause = 'cancelled',
                 terminal_at_ms = ?3
             WHERE session_id = ?1 AND batch_id = ?2 AND terminal_cause IS NULL";

        /// Remove session `?1`'s tombstones: host vacuum, the only reclaim a
        /// tombstone has short of session deletion. A vacuumed wake's redelivery
        /// still meets its receiver floor.
        delete_tombstones = "DELETE FROM queued_work_batches
             WHERE session_id = ?1 AND terminal_cause IS NOT NULL";

        /// Hand batch `?2` of session `?1` back open at its own position,
        /// under run `?3`, which must hold it.
        ///
        /// A row handed back to the queue is session work again: its writer
        /// wakes the session in the same transaction. A released wake keeps
        /// its redelivery floor: the fence is raised only by a wake's
        /// terminal transition.
        release_admitted = "UPDATE queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = ?1 AND batch_id = ?2 AND admitted_run = ?3";

        /// [`release_admitted`](Self::release_admitted) over every batch run
        /// `?2` of session `?1` still holds: the run's terminal write, after
        /// the settlements its commit named (FIG-3927).
        release_run = "UPDATE queued_work_batches
             SET admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = ?1 AND admitted_run = ?2 AND terminal_cause IS NULL";

        delete_by_session = "DELETE FROM queued_work_batches WHERE session_id = ?1";
    }
}
