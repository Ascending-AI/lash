//! Cross-table turn-ingress statements only PostgreSQL issues.

lash_store_sql::statements! {
    /// Statements over more than one of the family's tables, only PostgreSQL
    /// issues.
    pub(crate) struct TurnIngressPostgresStatements @ "turn_ingress" {
        /// Lock root `?2`'s bound inputs in queue order before a park write,
        /// so withdrawal either follows the park or prevents it.
        root_bound_input_states = "SELECT pti.state FROM session_root_inputs binding
             JOIN pending_turn_inputs pti
               ON pti.session_id = binding.session_id AND pti.input_id = binding.input_id
             WHERE binding.session_id = ?1 AND binding.root = ?2
             ORDER BY pti.enqueue_seq FOR UPDATE OF pti";

        /// Whether session `?1`'s root `?5` has checkpoint work for turn
        /// `?2` at the `after_work` checkpoint: rows step `?6` already bound
        /// (a re-executed step reads them back), open active-turn input while
        /// `?3` inputs may still be admitted, or a non-command item at the
        /// open boundary head while `?4` batches may.
        ///
        /// One probe, so one statement: the admission it guards takes a write
        /// transaction, and asking the halves separately would let a
        /// checkpoint open a transaction for work that had already gone. The
        /// boundary head is the one the candidate scan reads, because the
        /// probe must agree with the scan it decides for.
        checkpoint_work_pending_after_work = "WITH queued_work_head_candidate AS (
                 SELECT batch_id AS head_batch_id, delivery_policy AS head_delivery_policy
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn' AND admitted_root IS NULL
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             )
             SELECT EXISTS (
                SELECT 1 FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_root = ?5 AND admitted_by = ?6
             ) OR EXISTS (
                SELECT 1 FROM queued_work_batches
                WHERE session_id = ?1 AND admitted_root = ?5 AND admitted_by = ?6
             ) OR (
                ?3 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_root IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?2
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work')
                    LIMIT 1
                )
             ) OR (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_items AS item
                    JOIN queued_work_head_candidate AS head
                      ON head.head_batch_id = item.batch_id
                    WHERE head.head_delivery_policy = 'earliest_safe_boundary'
                      AND item.payload_json::jsonb ->> 'type' <> 'session_command'
                    LIMIT 1
                )
             )";

        /// [`checkpoint_work_pending_after_work`](Self::checkpoint_work_pending_after_work)
        /// at the `before_completion` checkpoint, which admits both minimum
        /// boundaries.
        checkpoint_work_pending_before_completion = "WITH queued_work_head_candidate AS (
                 SELECT batch_id AS head_batch_id, delivery_policy AS head_delivery_policy
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn' AND admitted_root IS NULL
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             )
             SELECT EXISTS (
                SELECT 1 FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_root = ?5 AND admitted_by = ?6
             ) OR EXISTS (
                SELECT 1 FROM queued_work_batches
                WHERE session_id = ?1 AND admitted_root = ?5 AND admitted_by = ?6
             ) OR (
                ?3 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_root IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?2
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work', 'before_completion')
                    LIMIT 1
                )
             ) OR (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_items AS item
                    JOIN queued_work_head_candidate AS head
                      ON head.head_batch_id = item.batch_id
                    WHERE head.head_delivery_policy = 'earliest_safe_boundary'
                      AND item.payload_json::jsonb ->> 'type' <> 'session_command'
                    LIMIT 1
                )
             )";

        /// The source key of batch `?2` of session `?1`, if root `?3` still
        /// holds it.
        ///
        /// The settlement observation needs the source key to decide whether a
        /// settled batch consumed a process wake (and so which fence to raise
        /// before the row goes away). The head payload is the shared
        /// [`QueuedBatchStatements::select_admitted_batch_head_payload`].
        select_admitted_batch_source_key = "SELECT source_key
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND admitted_root = ?3";
    }
}
