//! Cross-table turn-ingress statements only PostgreSQL issues.

lash_store_sql::statements! {
    /// Statements over more than one of the family's tables, only PostgreSQL
    /// issues.
    pub(crate) struct TurnIngressPostgresStatements @ "turn_ingress" {
        /// Whether session `?1` has checkpoint work for turn `?3` at generation
        /// `?2`, at the `after_work` checkpoint: an admitted active-turn input
        /// while `?5` inputs may still be claimed, or a non-command item behind
        /// the boundary head while `?6` batches may.
        ///
        /// The ready cutoff is `COALESCE(?4, <server clock>)`: the probe runs
        /// outside a transaction, so it cannot share a sampled
        /// `transaction_timestamp()` with a sibling statement the way the claim
        /// path does, and reading the clock separately would cost the hottest
        /// checkpoint path a round trip. `?4` is NULL in a production build —
        /// the store's test lease clock is the only thing that ever supplies
        /// it — so the statement falls through to the server clock it always
        /// read. The predicate keeps the indexed column on the left, so
        /// `idx_queued_work_batches_ready` is still seekable.
        checkpoint_work_pending_after_work = "WITH queued_work_unfiltered_head AS (
                 SELECT enqueue_seq AS head_enqueue_seq,
                        batch_id AS head_batch_id,
                        delivery_policy AS head_delivery_policy,
                        claim_id AS head_claim_id
                 FROM queued_work_batches
                 WHERE session_id = ?1
                   AND available_at_ms <= COALESCE(
                        ?4, FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000))
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
                     WHERE candidate.session_id = ?1
                       AND candidate.available_at_ms <= COALESCE(
                            ?4, FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000))
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
                ?5 > 0 AND EXISTS (
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
                ?6 > 0 AND EXISTS (
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
                 WHERE session_id = ?1
                   AND available_at_ms <= COALESCE(
                        ?4, FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000))
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
                     WHERE candidate.session_id = ?1
                       AND candidate.available_at_ms <= COALESCE(
                            ?4, FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000))
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
                ?5 > 0 AND EXISTS (
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
                ?6 > 0 AND EXISTS (
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
        /// PostgreSQL takes the wake source's advisory lock between reading a
        /// settling batch's wake identity and writing the redelivery fence, so
        /// it reads the two facts it needs — this and the head payload — and
        /// writes the fence separately. SQLite does all three in one statement
        /// under the write lock it already holds.
        select_claimed_batch_source_key = "SELECT source_key
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND claim_id = ?3
               AND claim_token = ?4";

        /// The first payload of batch `?2` of session `?1`, if claim `?3`/`?4`
        /// still holds it: the wake identity a settled batch contributes to its
        /// redelivery fence. Same fork as
        /// [`select_claimed_batch_source_key`](Self::select_claimed_batch_source_key).
        select_claimed_batch_head_payload = "SELECT item.payload_json
             FROM queued_work_batches AS batch
             JOIN queued_work_items AS item ON item.batch_id = batch.batch_id
             WHERE batch.session_id = ?1
               AND batch.batch_id = ?2
               AND batch.claim_id = ?3
               AND batch.claim_token = ?4
             ORDER BY item.item_index ASC
             LIMIT 1";
    }
}
