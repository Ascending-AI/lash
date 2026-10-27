//! Cross-table turn-ingress statements only PostgreSQL issues.

lash_store_sql::statements! {
    /// Statements over more than one of the family's tables, only PostgreSQL
    /// issues.
    pub(crate) struct TurnIngressPostgresStatements @ "turn_ingress" {
        /// Resolve the whole admission's identities under its session writer.
        /// The caller overlays rows it inserts so repeated drafts in this
        /// same batch retain their sequential idempotency semantics.
        input_admission_identities = "WITH wanted AS MATERIALIZED (SELECT value->>0 AS source_key, value->>1 AS input_id FROM jsonb_array_elements(?2::jsonb))
            SELECT 0, held.session_id, held.input_id, held.source_key, held.submission_digest
              FROM wanted JOIN pending_turn_inputs AS held
                ON held.session_id = ?1 AND held.source_key = wanted.source_key
            UNION
            SELECT 0, held.session_id, held.input_id, held.source_key, held.submission_digest
              FROM wanted JOIN pending_turn_inputs AS held ON held.input_id = wanted.input_id
            UNION
            SELECT 1, held.session_id, CAST(NULL AS TEXT), held.source_key, CAST(NULL AS TEXT)
              FROM wanted JOIN forsaken_turn_inputs AS held
                ON held.session_id = ?1 AND held.source_key = wanted.source_key";



        /// Lock run `?2`'s bound inputs in queue order before a park write,
        /// so withdrawal either follows the park or prevents it.
        run_bound_input_states = "SELECT pti.state FROM pending_turn_inputs pti
             WHERE pti.session_id = ?1 AND pti.admitted_run = ?2
             ORDER BY pti.enqueue_seq FOR UPDATE OF pti";

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
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_run IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?2
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work')
                    LIMIT 1
                )
             )";

        /// [`checkpoint_work_pending_after_work`](Self::checkpoint_work_pending_after_work)
        /// at the `before_completion` checkpoint, which admits both minimum
        /// boundaries.
        checkpoint_work_pending_before_completion = "SELECT EXISTS (
                SELECT 1 FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_run = ?4 AND admitted_by = ?5
             ) OR (
                ?3 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_run IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?2
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work', 'before_completion')
                    LIMIT 1
                )
             )";
    }
}
