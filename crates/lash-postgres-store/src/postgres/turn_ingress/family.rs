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

        /// Whether session `?1` has checkpoint work for turn `?3` at generation
        /// `?2`, at the `after_work` checkpoint: an admitted active-turn input
        /// while `?4` inputs may still be claimed, or a non-command item behind
        /// the boundary head while `?5` batches may.
        ///
        /// One probe, so one statement: the claim it guards takes a write
        /// transaction, and asking the two halves separately would let a
        /// checkpoint open a transaction for work that had already gone. The
        /// boundary head is the same common table expression the candidate
        /// scan uses, because the probe must agree with the scan it decides
        /// for.
        checkpoint_work_pending_after_work = "WITH queued_work_unfiltered_head AS (
                 SELECT enqueue_seq AS head_enqueue_seq,
                        batch_id AS head_batch_id,
                        delivery_policy AS head_delivery_policy,
                        claim_id AS head_claim_id
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn'
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?2
                   )
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             ),
             queued_work_head_candidate AS (
                 SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
                 FROM (
                     SELECT candidate.enqueue_seq AS head_enqueue_seq,
                            candidate.batch_id AS head_batch_id,
                            candidate.delivery_policy AS head_delivery_policy,
                            candidate.claim_id AS head_claim_id
                     FROM queued_work_batches AS candidate
                     CROSS JOIN queued_work_unfiltered_head AS unfiltered
                     WHERE candidate.session_id = ?1 AND candidate.work_kind = 'turn'
                       AND (
                            candidate.claim_token IS NULL
                            OR candidate.claim_session_lease_generation <> ?2
                       )
                       AND (
                            (
                                 candidate.enqueue_seq = unfiltered.head_enqueue_seq
                                 AND unfiltered.head_delivery_policy = 'earliest_safe_boundary'
                            )
                            OR (
                                 unfiltered.head_delivery_policy <> 'earliest_safe_boundary'
                                 AND unfiltered.head_claim_id IS NOT NULL
                                 AND candidate.claim_id IS DISTINCT FROM unfiltered.head_claim_id
                            )
                       )
                     ORDER BY candidate.enqueue_seq ASC
                     LIMIT 1
                 ) AS boundary_head
                 WHERE head_delivery_policy = 'earliest_safe_boundary'
             )
             SELECT (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{active_turn_input_state(state)}}
                      AND (claim_token IS NULL OR claim_session_lease_generation <> ?2)
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?3
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work')
                    LIMIT 1
                )
             ) OR (
                ?5 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_items AS item
                    JOIN queued_work_head_candidate AS head
                      ON head.head_batch_id = item.batch_id
                    WHERE item.payload_json::jsonb ->> 'type' <> 'session_command'
                    LIMIT 1
                )
             )";

        /// [`checkpoint_work_pending_after_work`](Self::checkpoint_work_pending_after_work)
        /// at the `before_completion` checkpoint, which admits both minimum
        /// boundaries.
        checkpoint_work_pending_before_completion = "WITH queued_work_unfiltered_head AS (
                 SELECT enqueue_seq AS head_enqueue_seq,
                        batch_id AS head_batch_id,
                        delivery_policy AS head_delivery_policy,
                        claim_id AS head_claim_id
                 FROM queued_work_batches
                 WHERE session_id = ?1 AND work_kind = 'turn'
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?2
                   )
                 ORDER BY enqueue_seq ASC
                 LIMIT 1
             ),
             queued_work_head_candidate AS (
                 SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
                 FROM (
                     SELECT candidate.enqueue_seq AS head_enqueue_seq,
                            candidate.batch_id AS head_batch_id,
                            candidate.delivery_policy AS head_delivery_policy,
                            candidate.claim_id AS head_claim_id
                     FROM queued_work_batches AS candidate
                     CROSS JOIN queued_work_unfiltered_head AS unfiltered
                     WHERE candidate.session_id = ?1 AND candidate.work_kind = 'turn'
                       AND (
                            candidate.claim_token IS NULL
                            OR candidate.claim_session_lease_generation <> ?2
                       )
                       AND (
                            (
                                 candidate.enqueue_seq = unfiltered.head_enqueue_seq
                                 AND unfiltered.head_delivery_policy = 'earliest_safe_boundary'
                            )
                            OR (
                                 unfiltered.head_delivery_policy <> 'earliest_safe_boundary'
                                 AND unfiltered.head_claim_id IS NOT NULL
                                 AND candidate.claim_id IS DISTINCT FROM unfiltered.head_claim_id
                            )
                       )
                     ORDER BY candidate.enqueue_seq ASC
                     LIMIT 1
                 ) AS boundary_head
                 WHERE head_delivery_policy = 'earliest_safe_boundary'
             )
             SELECT (
                ?4 > 0 AND EXISTS (
                    SELECT 1
                    FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND {{active_turn_input_state(state)}}
                      AND (claim_token IS NULL OR claim_session_lease_generation <> ?2)
                      AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                      AND ingress_json::jsonb ->> 'turn_id' = ?3
                      AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                          IN ('after_work', 'before_completion')
                    LIMIT 1
                )
             ) OR (
                ?5 > 0 AND EXISTS (
                    SELECT 1
                    FROM queued_work_items AS item
                    JOIN queued_work_head_candidate AS head
                      ON head.head_batch_id = item.batch_id
                    WHERE item.payload_json::jsonb ->> 'type' <> 'session_command'
                    LIMIT 1
                )
             )";

        /// The source key of batch `?2` of session `?1`, if claim `?3`/`?4`
        /// still holds it.
        ///
        /// The settlement observation needs the source key to decide whether a
        /// settled batch consumed a process wake (and so which fence to raise
        /// before the row goes away). The head payload is the shared
        /// [`QueuedBatchStatements::select_claimed_batch_head_payload`].
        select_claimed_batch_source_key = "SELECT source_key
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND claim_id = ?3
               AND claim_token = ?4";
    }
}
