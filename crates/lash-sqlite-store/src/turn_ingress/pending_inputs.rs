//! `pending_turn_inputs` statements only SQLite issues.
//!
//! Three things fork on this table. SQLite reads JSON with `json_extract` where
//! PostgreSQL casts to `jsonb` and uses `->>`; SQLite binds a list as a JSON
//! array and unpacks it with `json_each` where PostgreSQL binds a real array;
//! and SQLite takes no row lock at all, because every write path in this crate
//! already runs inside `BEGIN IMMEDIATE` and holds the database write lock for
//! the whole transaction.
//!
//! Every admission scan seeks `idx_pending_turn_inputs_open_state`, the partial
//! index over undelivered states. SQLite uses it only when the query repeats
//! the index's state set, which the vocabulary token renders to exactly the
//! schema's predicate. Admission also tests `admitted_root IS NULL` so a root
//! cannot bind an input another root already holds.

lash_store_sql::statements! {
    /// `pending_turn_inputs` statements only SQLite issues.
    pub(crate) struct PendingInputSqliteStatements @ "pending_turn_input" {
        /// The facts the settlement verdict consults about input `?2` of
        /// session `?1`.
        ///
        /// No lock suffix: the commit already holds the database write lock.
        /// PostgreSQL must take the row lock explicitly.
        settlement_facts = "SELECT admitted_root, state
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// Session `?1`'s inputs from `?2` onwards: the suffix a cancel
        /// anchored at one input covers. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_suffix = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s open active-turn inputs, which an interrupted
        /// turn's commit re-defers: input addressed to the turn that no
        /// checkpoint admitted. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_pending_active = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s open next-turn inputs a root's admission composes
        /// from, up to `?2` of them (ADR 0101 §4, §5).
        ///
        /// The admission chose the turn lane at a boundary whose command
        /// lane was empty, so a command enqueued since holds back only the
        /// rows after it: the prefix ends at the earliest open command. It
        /// also ends at the earliest open queued turn work, because the turn
        /// lane is one FIFO over both admission tables and no input accepted
        /// after it is taken past it. PostgreSQL takes `FOR UPDATE` here;
        /// SQLite is already the only writer.
        admission_candidates_next_turn = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
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
             LIMIT ?2";

        /// Session `?1`'s open input for active turn `?3`, up to `?2` of
        /// them, at the `after_work` checkpoint.
        ///
        /// One statement per checkpoint because the admitted minimum-boundary
        /// set is what the checkpoint decides, and an optional predicate over a
        /// bound boundary — `COALESCE(?N, min_boundary)` or `?N IS NULL OR …` —
        /// cannot seek. The two checkpoints are picked by an exhaustive match,
        /// so a third would not compile.
        admission_candidates_active_turn_after_work = "SELECT enqueue_seq, input_id, session_id,
                    source_key, ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
               AND json_extract(ingress_json, '$.scope') = 'active_turn'
               AND json_extract(ingress_json, '$.turn_id') = ?3
               AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                   IN ('after_work')
             ORDER BY enqueue_seq ASC
             LIMIT ?2";

        /// [`admission_candidates_active_turn_after_work`](Self::admission_candidates_active_turn_after_work)
        /// at the `before_completion` checkpoint, which admits both boundaries.
        admission_candidates_active_turn_before_completion = "SELECT enqueue_seq, input_id,
                    session_id, source_key, ingress_json, state, input_json, enqueued_at_ms,
                    admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{pending_active_turn_input_state(state)}}
               AND json_extract(ingress_json, '$.scope') = 'active_turn'
               AND json_extract(ingress_json, '$.turn_id') = ?3
               AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                   IN ('after_work', 'before_completion')
             ORDER BY enqueue_seq ASC
             LIMIT ?2";
    }
}
