//! `queued_work_batches` statements only PostgreSQL issues.
//!
//! The claim scan is where the two backends diverge most. PostgreSQL takes
//! `FOR UPDATE … SKIP LOCKED` over the candidate rows so two runners of
//! different sessions never queue behind each other, and compares a claimed
//! head with `IS DISTINCT FROM` because a NULL claim must compare unequal;
//! SQLite spells the same rule with an explicit `IS NULL OR <>` and needs no
//! row lock under `BEGIN IMMEDIATE`.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only PostgreSQL issues.
    pub(crate) struct QueuedBatchPostgresStatements @ "queued_work_batch" {
        insert_new = "INSERT INTO queued_work_batches (
                 enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
                 authority_json, merge_key, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (session_id, source_key) DO NOTHING
             RETURNING batch_id";

        /// The facts the settlement verdict consults about batch `?2` of
        /// session `?1`, locked for the caller's transaction.
        settlement_facts = "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2 LIMIT 1 FOR UPDATE";

        /// Batch `?2` of session `?1` at `?3`, if no live claim holds it,
        /// locked for the caller's transaction.
        select_cancelable = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM session_execution_leases sel
                    WHERE sel.session_id = ?1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > ?3
                      AND sel.lease_fencing_token
                          = queued_work_batches.claim_session_lease_generation
               ))
             FOR UPDATE";

        /// Cancel batch `?1`.
        ///
        /// Keyed by id alone because the row lock
        /// [`select_cancelable`](Self::select_cancelable) took is what holds the
        /// liveness decision; SQLite has no row lock, so it repeats the
        /// predicate on its delete.
        delete_cancelled = "DELETE FROM queued_work_batches WHERE batch_id = ?1";

        /// Session `?1`'s claim candidates for generation `?2`, up to `?3` of
        /// them, with no turn in progress.
        claim_candidates_idle = "WITH queued_work_head_candidate AS (
                 SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
                 FROM (
                     SELECT enqueue_seq AS head_enqueue_seq,
                            batch_id AS head_batch_id,
                            delivery_policy AS head_delivery_policy,
                            claim_id AS head_claim_id
                     FROM queued_work_batches
                     WHERE session_id = ?1
                       AND (
                            claim_token IS NULL
                            OR claim_session_lease_generation <> ?2
                       )
                     ORDER BY CASE WHEN work_kind = 'control' THEN 0 ELSE 1 END, enqueue_seq ASC
                     LIMIT 1
                 ) AS unfiltered_head
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND (work_kind = 'control' OR NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS commands
                    WHERE commands.session_id = ?1 AND commands.work_kind = 'control'
               ))
               AND enqueue_seq >= head_enqueue_seq
               AND work_kind = (SELECT work_kind FROM queued_work_batches WHERE session_id = ?1 AND enqueue_seq = head_enqueue_seq)
               AND (head_claim_id IS NULL OR queued_work_batches.claim_id = head_claim_id)
             ORDER BY CASE WHEN work_kind = 'control' THEN 0 ELSE 1 END, enqueue_seq ASC
             LIMIT COALESCE((
                 SELECT CASE WHEN head_claim_id IS NULL THEN ?3 ELSE 9223372036854775807 END
                 FROM queued_work_head_candidate
             ), 0)
             FOR UPDATE OF queued_work_batches";

        /// [`claim_candidates_idle`](Self::claim_candidates_idle) at a turn
        /// checkpoint, where only work whose delivery policy admits the
        /// earliest safe boundary may start.
        claim_candidates_boundary = "WITH queued_work_unfiltered_head AS (
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
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1 AND work_kind = 'turn'
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND enqueue_seq >= head_enqueue_seq
               AND (head_claim_id IS NULL OR queued_work_batches.claim_id = head_claim_id)
             ORDER BY enqueue_seq ASC
             LIMIT COALESCE((
                 SELECT CASE WHEN head_claim_id IS NULL THEN ?3 ELSE 9223372036854775807 END
                 FROM queued_work_head_candidate
             ), 0)
             FOR UPDATE OF queued_work_batches SKIP LOCKED";

        /// Which of the batch ids in the array `?2` session `?1` still holds.
        select_present_ids = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ANY(?2)";

        /// Session `?1`'s unclaimed batches for generation `?2` among the ids
        /// in the array `?3`.
        select_by_ids = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?2)
               AND batch_id = ANY(?3)
             ORDER BY enqueue_seq ASC";

        /// The same rows keyed by the claim ids in the array `?3`: an exact
        /// claim must validate every batch the interrupted claim it recomposes
        /// covered, not only the ones it was asked for.
        select_by_claim_ids = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key,
                    enqueued_at_ms, claim_fencing_token, claim_token,
                    claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?2)
               AND claim_id = ANY(?3)
             ORDER BY enqueue_seq ASC";

        /// The batch form of the shared `abandon_claim`, over the claims bound
        /// as the five parallel arrays `?1` to `?5`.
        ///
        /// SQLite loops the single-row statement inside one write transaction
        /// instead; it holds the database lock for the whole loop, so the
        /// batch is atomic either way and only PostgreSQL saves round trips by
        /// writing it as one statement.
        abandon_claims = "UPDATE queued_work_batches AS batch
             SET claim_id = abandoned.restore_claim_id,
                 claim_token = abandoned.restore_claim_token,
                 claim_session_lease_generation = 0
             FROM unnest(?1::TEXT[], ?2::TEXT[], ?3::TEXT[], ?4::TEXT[], ?5::TEXT[])
                  AS abandoned(session_id, claim_id, claim_token,
                               restore_claim_id, restore_claim_token)
             WHERE batch.session_id = abandoned.session_id
               AND batch.claim_id = abandoned.claim_id
               AND batch.claim_token = abandoned.claim_token";
    }
}

lash_store_sql::statements! {
    /// `queued_work_items` statements only PostgreSQL issues.
    pub(crate) struct QueuedItemPostgresStatements @ "queued_work_item" {
        /// SQLite gets this from the foreign key's `ON DELETE CASCADE`; this
        /// schema's constraint is not declared cascading, so the sweep names
        /// the rows itself.
        delete_by_session = "DELETE FROM queued_work_items
             WHERE batch_id IN (
                 SELECT batch_id FROM queued_work_batches WHERE session_id = ?1
             )";
    }
}
