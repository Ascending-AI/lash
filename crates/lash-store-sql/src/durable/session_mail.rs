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
        /// The vocabulary predicate matches the stores' open-bound-run partial
        /// indexes: retained settled bindings never lengthen this probe.
        bound_run = "SELECT admitted_run FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NOT NULL
               AND {{nonterminal_turn_input_state(state)}}
             UNION ALL
             SELECT admitted_run FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run IS NOT NULL
             LIMIT 1";

        /// Session `?1`'s open, unbound inputs that a turn may admit, in
        /// ingress order: next-turn inputs, and active-turn inputs with the
        /// delivery they name, and the cause each retained.
        open_inputs = "SELECT input_id, enqueue_seq, source_key, run_spec_hash, state, ingress_json,
                    trace_cause_json
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NULL
               AND state IN ('deferred_next_turn', 'pending_active')
             ORDER BY enqueue_seq";

        /// The unbound active-turn inputs the cancellation transaction may
        /// affect, in ingress order. The owner decodes each immutable address
        /// before applying the cancelled run's disposition.
        cancel_inputs = "SELECT input_id, input_json, ingress_json
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NULL AND state = 'pending_active'
             ORDER BY enqueue_seq";

        /// Session `?1`'s open, unbound queued work batches, in ingress
        /// order, each with whether a host requested its entered operation
        /// run's cancel.
        open_batches = "SELECT batch_id, enqueue_seq, delivery_policy, payload_json,
                    CASE WHEN cancel_request_json IS NULL THEN 0 ELSE 1 END
             FROM queued_work_batches
             WHERE session_id = ?1 AND admitted_run IS NULL AND terminal_cause IS NULL
             ORDER BY enqueue_seq";

        /// Enter the operation run of open plugin task `?2` of session `?1`
        /// at `?3`, keeping a first entry; no row when it is no longer open.
        enter_operation = "UPDATE queued_work_batches
             SET entered_at_ms = COALESCE(entered_at_ms, ?3)
             WHERE session_id = ?1 AND batch_id = ?2
               AND admitted_run IS NULL AND terminal_cause IS NULL
             RETURNING batch_id";

        /// Open command `?2` of session `?1`: its payload, and the request
        /// its entered operation run accepted, if any.
        open_operation = "SELECT payload_json, cancel_request_json
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2
               AND admitted_run IS NULL AND terminal_cause IS NULL";

        /// Withdraw open command `?2` of session `?1` into its `cancelled`
        /// tombstone at `?3` while no operation run entered it; no row once
        /// one did.
        withdraw_unentered = "UPDATE queued_work_batches
             SET terminal_cause = 'cancelled', terminal_at_ms = ?3
             WHERE session_id = ?1 AND batch_id = ?2
               AND admitted_run IS NULL AND terminal_cause IS NULL
               AND entered_at_ms IS NULL
             RETURNING batch_id";

        /// Record request `?3` as the cancel of the entered operation run of
        /// open command `?2` of session `?1`; no row when it was not entered
        /// or already holds a request.
        request_operation_cancel = "UPDATE queued_work_batches
             SET cancel_request_json = ?3
             WHERE session_id = ?1 AND batch_id = ?2
               AND admitted_run IS NULL AND terminal_cause IS NULL
               AND entered_at_ms IS NOT NULL AND cancel_request_json IS NULL
             RETURNING batch_id";

        /// Bind input `?2` of session `?1` to run `?3`, its admitting owner,
        /// or keep it bound there: a running run's every phase rebinds the
        /// steering input its checkpoints delivered. No row when it is no
        /// longer open, or another run holds it.
        bind_input = "UPDATE pending_turn_inputs
             SET admitted_run = ?3, admitted_by = ?3
             WHERE session_id = ?1 AND input_id = ?2
               AND (admitted_run IS NULL OR admitted_run = ?3)
               AND state IN ('deferred_next_turn', 'pending_active')
             RETURNING input_id";

        /// Session `?1`'s open, unbound inputs whose id or source key is
        /// `?2`, in ingress order: the candidates for the input the session
        /// admits as run `?2`, which the naming rule then picks among.
        open_inputs_named = "SELECT input_id, source_key
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run IS NULL
               AND state IN ('deferred_next_turn', 'pending_active')
               AND (input_id = ?2 OR source_key = ?2)
             ORDER BY enqueue_seq";

        /// Withdraw input `?2` of session `?1` into state `?3` at `?4`, its
        /// tombstone, while it is still open and unbound; no row when an
        /// admission bound it first.
        withdraw_input = "UPDATE pending_turn_inputs
             SET state = ?3, terminal_at_ms = ?4
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_run IS NULL
               AND state IN ('deferred_next_turn', 'pending_active')
             RETURNING input_id";

        /// Settle the inputs run `?2` of session `?1` still holds as it
        /// ends with no commit that settled them (a cancelled turn): into
        /// state `?3` at `?4`.
        settle_held_inputs = "UPDATE pending_turn_inputs
             SET state = ?3, terminal_at_ms = ?4, admitted_by = NULL
             WHERE session_id = ?1 AND admitted_run = ?2 AND {{nonterminal_turn_input_state(state)}}";

        /// Settle the batches run `?2` of session `?1` still holds as it
        /// ends with no commit that settled them: into their `cancelled`
        /// tombstone at `?3`.
        settle_held_batches = "UPDATE queued_work_batches
             SET admitted_run = NULL, admitted_by = NULL,
                 terminal_cause = 'cancelled', terminal_at_ms = ?3
             WHERE session_id = ?1 AND admitted_run = ?2 AND terminal_cause IS NULL";

        /// Bind batch `?2` of session `?1` to run `?3`, its admitting owner,
        /// or keep it bound there: a running run's every phase rebinds the
        /// queued turn work its checkpoints delivered. No row when it is no
        /// longer open, or another run holds it.
        bind_batch = "UPDATE queued_work_batches
             SET admitted_run = ?3, admitted_by = ?3
             WHERE session_id = ?1 AND batch_id = ?2
               AND (admitted_run IS NULL OR admitted_run = ?3)
               AND terminal_cause IS NULL
             RETURNING batch_id";
    }
}
