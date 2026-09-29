//! `pending_turn_inputs` statements only PostgreSQL issues.
//!
//! Three things fork on this table. PostgreSQL reads JSON by casting to `jsonb`
//! and using `->>` where SQLite calls `json_extract`; it binds a list as a real
//! array where SQLite binds a JSON array; and it must take row locks
//! explicitly, because check-then-act on a row is not atomic under READ
//! COMMITTED and SQLite already holds the database write lock.

lash_store_sql::statements! {
    /// `pending_turn_inputs` statements only PostgreSQL issues.
    pub(crate) struct PendingInputPostgresStatements @ "pending_turn_input" {
        /// At most `?2` ingress obligations due at `?1`, oldest due first,
        /// each row locked for the caller's claim and skipped by every
        /// concurrent claimant: two deployments' relays take disjoint pages
        /// (ADR 0109 §1.7).
        obligation_select_due_locking = "SELECT obligation_id FROM pending_turn_inputs
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";

        /// Input `?2` of session `?1`, locked for the caller's transaction.
        select_by_id_for_update = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2 FOR UPDATE";

        /// The input session `?1` filed under source key `?2`, locked for the
        /// caller's transaction.
        select_by_source_key_for_update = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2 FOR UPDATE";

        /// The facts the settlement verdict consults about input `?2` of
        /// session `?1`, locked for the caller's transaction.
        settlement_facts = "SELECT admitted_root, state
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2 LIMIT 1 FOR UPDATE";

        /// Session `?1`'s inputs from `?2` onwards, locked: the suffix a cancel
        /// anchored at one input covers.
        select_suffix = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s open active-turn inputs, locked, which an
        /// interrupted turn's commit re-defers: input addressed to the turn
        /// that no checkpoint admitted.
        select_pending_active = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s open next-turn inputs a root's admission composes
        /// from, up to `?2` of them, waiting for locked rows so the head
        /// cannot be skipped (ADR 0101 §4, §5).
        ///
        /// The admission chose the turn lane at a boundary whose command
        /// lane was empty, so a command enqueued since holds back only the
        /// rows after it: the prefix ends at the earliest open command. It
        /// also ends at the earliest open queued turn work, because the turn
        /// lane is one FIFO over both admission tables.
        admission_candidates_next_turn = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{deferred_next_turn_turn_input_state(state)}}
               AND NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS commands
                    WHERE commands.session_id = ?1 AND commands.work_kind = 'control'
                      AND commands.enqueue_seq < pending_turn_inputs.enqueue_seq
               )
               AND NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS turn_work
                    WHERE turn_work.session_id = ?1 AND turn_work.work_kind = 'turn'
                      AND turn_work.admitted_root IS NULL
                      AND turn_work.enqueue_seq < pending_turn_inputs.enqueue_seq
               )
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE";

        /// Session `?1`'s open input for active turn `?3`, up to `?2` of
        /// them, at the `after_work` checkpoint.
        ///
        /// One statement per checkpoint because the admitted minimum-boundary
        /// set is what the checkpoint decides, and an optional predicate over a
        /// bound boundary cannot seek the open-row index.
        admission_candidates_active_turn_after_work = "SELECT enqueue_seq, input_id, session_id,
                    source_key, ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
               AND ingress_json::jsonb ->> 'scope' = 'active_turn'
               AND ingress_json::jsonb ->> 'turn_id' = ?3
               AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                   IN ('after_work')
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";

        /// [`admission_candidates_active_turn_after_work`](Self::admission_candidates_active_turn_after_work)
        /// at the `before_completion` checkpoint, which admits both boundaries.
        admission_candidates_active_turn_before_completion = "SELECT enqueue_seq, input_id,
                    session_id, source_key, ingress_json, state, input_json, enqueued_at_ms,
                    admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
               AND ingress_json::jsonb ->> 'scope' = 'active_turn'
               AND ingress_json::jsonb ->> 'turn_id' = ?3
               AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                   IN ('after_work', 'before_completion')
             ORDER BY enqueue_seq ASC
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";

        /// Lock, in queue order, cancel targets `?2` of session `?1`.
        ///
        /// A cancel may write several rows, and every other multi-row writer
        /// of them locks in queue order. Taking the whole set in that order
        /// first is what keeps concurrent writers from deadlocking.
        lock_cancel_targets_in_queue_order = "SELECT enqueue_seq
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND input_id = ANY(?2::TEXT[])
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// [`lock_cancel_targets_in_queue_order`](Self::lock_cancel_targets_in_queue_order)
        /// for the suffix of session `?1` from `enqueue_seq` `?2`.
        lock_cancel_suffix_in_queue_order = "SELECT enqueue_seq
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC
             FOR UPDATE";
    }
}
