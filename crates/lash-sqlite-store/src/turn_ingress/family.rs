//! Cross-table turn-ingress statements only SQLite issues.

lash_store_sql::statements! {
    /// Statements over more than one of the family's tables, only SQLite
    /// issues.
    pub(crate) struct TurnIngressSqliteStatements @ "turn_ingress" {
        /// States of inputs bound to root `?2` in session `?1`, read under
        /// the write transaction that records its park.
        root_bound_input_states = "SELECT pti.state FROM session_root_inputs binding
             JOIN pending_turn_inputs pti
               ON pti.session_id = binding.session_id AND pti.input_id = binding.input_id
             WHERE binding.session_id = ?1 AND binding.root = ?2
             ORDER BY pti.input_id";

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
                    FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_root IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND json_extract(ingress_json, '$.scope') = 'active_turn'
                      AND json_extract(ingress_json, '$.turn_id') = ?2
                      AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                          IN ('after_work')
                    LIMIT 1
                )
             ) OR (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_head_candidate AS head
                    JOIN queued_work_items AS item
                      ON item.batch_id = head.head_batch_id
                    WHERE head.head_delivery_policy = 'earliest_safe_boundary'
                      AND json_extract(item.payload_json, '$.type') <> 'session_command'
                    LIMIT 1
                )
             )";

        /// [`checkpoint_work_pending_after_work`](Self::checkpoint_work_pending_after_work)
        /// at the `before_completion` checkpoint, which admits both minimum
        /// boundaries. One statement per checkpoint for the reason the
        /// candidate scan has one: an optional boundary predicate cannot seek.
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
                    FROM pending_turn_inputs INDEXED BY idx_pending_turn_inputs_open
                    WHERE session_id = ?1
                      AND {{undelivered_turn_input_state(state)}}
                      AND admitted_root IS NULL
                      AND {{pending_active_turn_input_state(state)}}
                      AND json_extract(ingress_json, '$.scope') = 'active_turn'
                      AND json_extract(ingress_json, '$.turn_id') = ?2
                      AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                          IN ('after_work', 'before_completion')
                    LIMIT 1
                )
             ) OR (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_head_candidate AS head
                    JOIN queued_work_items AS item
                      ON item.batch_id = head.head_batch_id
                    WHERE head.head_delivery_policy = 'earliest_safe_boundary'
                      AND json_extract(item.payload_json, '$.type') <> 'session_command'
                    LIMIT 1
                )
             )";
    }
}
