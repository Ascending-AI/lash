//! The session's mailbox: the neutral statements of the `session_mail` domain (L3s, FIG-5196).
//!
//! The rows are the session ingress tables' (`pending_turn_inputs`,
//! `queued_work_batches`); this domain reads the open ones on every claim of
//! the session actor and binds what it admits inside the owner commit. This
//! file and the matching file in each dialect are the only places its SQL may
//! appear (`scripts/check-durable-sql.py`).

crate::statements! {
    /// `session_mail` statements both backends issue verbatim.
    pub struct SessionMailStatements @ "durable_session_mail" {
        /// Session `?1`'s standing: whether it has a catalog row, whether it
        /// was deleted and whether its close began.
        standing = "SELECT
                 (SELECT COUNT(*) FROM session_meta WHERE session_id = ?1),
                 (SELECT COUNT(*) FROM deleted_sessions WHERE session_id = ?1),
                 (SELECT COUNT(*) FROM session_meta
                     WHERE session_id = ?1 AND closing_intent IS NOT NULL)";

        /// The run session `?1`'s bound, unsettled ingress names, if any.
        bound_run = "SELECT admitted_run FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NOT NULL
             UNION ALL
             SELECT admitted_run FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run IS NOT NULL
             LIMIT 1";

        /// Session `?1`'s open, unbound inputs that a turn may admit, in
        /// ingress order: next-turn inputs, and active-turn inputs with the
        /// delivery they name.
        open_inputs = "SELECT input_id, enqueue_seq, source_key, run_spec_hash, state, ingress_json
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NULL
               AND state IN ('deferred_next_turn', 'pending_active')
             ORDER BY enqueue_seq";

        /// Session `?1`'s open, unbound queued work batches, in ingress order.
        open_batches = "SELECT batch_id, enqueue_seq, work_kind, delivery_policy, payload_json
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run IS NULL AND terminal_cause IS NULL
             ORDER BY enqueue_seq";

        /// Bind input `?2` of session `?1` to run `?3`, its admitting owner;
        /// no row when it is no longer open and unbound.
        bind_input = "UPDATE pending_turn_inputs
             SET admitted_run = ?3, admitted_by = ?3
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_run IS NULL
               AND state IN ('deferred_next_turn', 'pending_active')
             RETURNING input_id";

        /// Record that run `?3` of session `?1` executes input `?2`.
        record_run_input = "INSERT INTO session_run_inputs (session_id, input_id, run)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, input_id) DO NOTHING";

        /// Settle the inputs run `?2` of session `?1` still holds as it
        /// ends with no commit that settled them (a cancelled turn): into
        /// state `?3` at `?4`.
        settle_held_inputs = "UPDATE pending_turn_inputs
             SET state = ?3, terminal_at_ms = ?4, admitted_run = NULL, admitted_by = NULL
             WHERE session_id = ?1 AND admitted_run = ?2";

        /// Settle the batches run `?2` of session `?1` still holds as it
        /// ends with no commit that settled them: into their `cancelled`
        /// tombstone at `?3`.
        settle_held_batches = "UPDATE queued_work_batches
             SET admitted_run = NULL, admitted_by = NULL,
                 terminal_cause = 'cancelled', terminal_at_ms = ?3
             WHERE session_id = ?1 AND admitted_run = ?2 AND terminal_cause IS NULL";

        /// Bind batch `?2` of session `?1` to run `?3`, its admitting owner;
        /// no row when it is no longer open and unbound.
        bind_batch = "UPDATE queued_work_batches
             SET admitted_run = ?3, admitted_by = ?3
             WHERE session_id = ?1 AND batch_id = ?2 AND admitted_run IS NULL
               AND terminal_cause IS NULL
             RETURNING batch_id";
    }
}
