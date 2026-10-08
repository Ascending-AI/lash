//! Cross-table turn-ingress statements only SQLite issues.

lash_store_sql::statements! {
    /// Statements over more than one of the family's tables, only SQLite
    /// issues.
    pub(crate) struct TurnIngressSqliteStatements @ "turn_ingress" {
        /// States of inputs bound to run `?2` in session `?1`, read under
        /// the write transaction that records its park.
        run_bound_input_states = "SELECT pti.state FROM session_run_inputs binding
             JOIN pending_turn_inputs pti
               ON pti.session_id = binding.session_id AND pti.input_id = binding.input_id
             WHERE binding.session_id = ?1 AND binding.run = ?2
             ORDER BY pti.input_id";

        /// Whether session `?1`'s run `?4` has checkpoint input for turn
        /// `?2` at `after_work`: rows step `?5` already bound, or open
        /// active-turn input while `?3` inputs may still be admitted.
        ///
        /// One probe matches the input scan in the admission transaction.
        checkpoint_work_pending_after_work = "SELECT EXISTS (
                SELECT 1 FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_run = ?4 AND admitted_by = ?5
             ) OR (
                ?3 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_run IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND json_extract(ingress_json, '$.scope') = 'active_turn'
                      AND json_extract(ingress_json, '$.turn_id') = ?2
                      AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                          IN ('after_work')
                    LIMIT 1
                )
             )";

        /// [`checkpoint_work_pending_after_work`](Self::checkpoint_work_pending_after_work)
        /// at the `before_completion` checkpoint, which admits both minimum
        /// boundaries. One statement per checkpoint for the reason the
        /// candidate scan has one: an optional boundary predicate cannot seek.
        checkpoint_work_pending_before_completion = "SELECT EXISTS (
                SELECT 1 FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_run = ?4 AND admitted_by = ?5
             ) OR (
                ?3 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open_state
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_run IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND json_extract(ingress_json, '$.scope') = 'active_turn'
                      AND json_extract(ingress_json, '$.turn_id') = ?2
                      AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                          IN ('after_work', 'before_completion')
                    LIMIT 1
                )
             )";
    }
}
