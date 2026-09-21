//! `queued_work_batches` statements only PostgreSQL issues.
//!
//! The claim scan is where the two backends diverge most. PostgreSQL takes
//! `FOR UPDATE … SKIP LOCKED` over the candidate rows so two runners of
//! different sessions never queue behind each other, and compares a claimed
//! head with `IS DISTINCT FROM` because a NULL claim must compare unequal;
//! SQLite spells the same rule with an explicit `IS NULL OR <>` and needs no
//! row lock under `BEGIN IMMEDIATE`.
//!
//! The ready cutoff is bound rather than read inline. Every caller already
//! samples it with `postgres_transaction_epoch_ms`, which is
//! `transaction_timestamp()` — the same instant the statement would have read —
//! and binding it is what lets the scan be one fixed string instead of a
//! `concat!` that forks between the production and `testing` builds.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only PostgreSQL issues.
    pub(crate) struct QueuedBatchPostgresStatements @ "queued_work_batch" {
        /// The next `enqueue_seq` for this table, drawn from the column's own
        /// sequence before the insert.
        ///
        /// `pg_get_serial_sequence` takes its relation as *text*, so this is
        /// the one statement in the family whose table name is spelled with
        /// the `lash_` prefix rather than rendered: the renderer rewrites
        /// table *tokens*, and a name inside a string literal is not one. It
        /// is still a named statement with one owner, which is what the
        /// alternative — a literal at the call site — was not.
        select_next_enqueue_seq = "SELECT nextval(pg_get_serial_sequence(
                 'lash_queued_work_batches',
                 'enqueue_seq'
             ))";

        /// Enqueue batch `?2` for session `?3` at sequence `?1`, keeping an
        /// existing batch under the same source key and returning the id that
        /// was written.
        insert_new = "INSERT INTO queued_work_batches (
                 enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
                 authority_json, merge_key, available_at_ms, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
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

        /// Session `?1`'s ready head at `?2`, for generation `?3`, unfiltered
        /// by the delivery boundary: what an empty candidate scan is asked
        /// about so the refusal it reports names the head's own reason.
        select_head_candidate = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key, available_at_ms,
                    enqueued_at_ms, claim_fencing_token, claim_token,
                    claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?3
               )
             ORDER BY enqueue_seq ASC
             LIMIT 1";

        /// Whether session `?1` holds work that is not yet available at `?2`
        /// for generation `?3`: the difference between an exhausted lane and a
        /// waiting one.
        exists_deferred = "SELECT EXISTS (
                 SELECT 1
                 FROM queued_work_batches
                 WHERE session_id = ?1
                   AND available_at_ms > ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?3
                   )
             )";

        /// Session `?1`'s claim candidates at `?2` for generation `?3`, up to
        /// `?4` of them, with no turn in progress.
        claim_candidates_idle = "WITH queued_work_head_candidate AS (
                 SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
                 FROM (
                     SELECT enqueue_seq AS head_enqueue_seq,
                            batch_id AS head_batch_id,
                            delivery_policy AS head_delivery_policy,
                            claim_id AS head_claim_id
                     FROM queued_work_batches
                     WHERE session_id = ?1
                       AND available_at_ms <= ?2
                       AND (
                            claim_token IS NULL
                            OR claim_session_lease_generation <> ?3
                       )
                     ORDER BY enqueue_seq ASC
                     LIMIT 1
                 ) AS unfiltered_head
             )
             SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?3
               )
               AND enqueue_seq >= head_enqueue_seq
               AND (head_claim_id IS NULL OR queued_work_batches.claim_id = head_claim_id)
             ORDER BY enqueue_seq ASC
             LIMIT COALESCE((
                 SELECT CASE WHEN head_claim_id IS NULL THEN ?4 ELSE 9223372036854775807 END
                 FROM queued_work_head_candidate
             ), 0)
             FOR UPDATE OF queued_work_batches SKIP LOCKED";

        /// [`claim_candidates_idle`](Self::claim_candidates_idle) at a turn
        /// checkpoint, where only work whose delivery policy admits the
        /// earliest safe boundary may start.
        claim_candidates_boundary = "WITH queued_work_unfiltered_head AS (
                 SELECT enqueue_seq AS head_enqueue_seq,
                        batch_id AS head_batch_id,
                        delivery_policy AS head_delivery_policy,
                        claim_id AS head_claim_id
                 FROM queued_work_batches
                 WHERE session_id = ?1
                   AND available_at_ms <= ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?3
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
                       AND candidate.available_at_ms <= ?2
                       AND (
                            candidate.claim_token IS NULL
                            OR candidate.claim_session_lease_generation <> ?3
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
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             CROSS JOIN queued_work_head_candidate
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?3
               )
               AND enqueue_seq >= head_enqueue_seq
               AND (head_claim_id IS NULL OR queued_work_batches.claim_id = head_claim_id)
             ORDER BY enqueue_seq ASC
             LIMIT COALESCE((
                 SELECT CASE WHEN head_claim_id IS NULL THEN ?4 ELSE 9223372036854775807 END
                 FROM queued_work_head_candidate
             ), 0)
             FOR UPDATE OF queued_work_batches SKIP LOCKED";

        /// Which of the batch ids in the array `?2` session `?1` still holds.
        select_present_ids = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ANY(?2)";

        /// Session `?1`'s unclaimed-at-`?2` batches for generation `?3` among
        /// the ids in the array `?4`.
        select_by_ids = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND batch_id = ANY(?4)
             ORDER BY enqueue_seq ASC";

        /// The same rows keyed by the claim ids in the array `?4`: an exact
        /// claim must validate every batch the interrupted claim it recomposes
        /// covered, not only the ones it was asked for.
        select_by_claim_ids = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key, available_at_ms,
                    enqueued_at_ms, claim_fencing_token, claim_token,
                    claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND claim_id = ANY(?4)
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s unclaimed-at-`?2` batches for generation `?3` whose
        /// `enqueue_seq` lies between `?4` and `?5`: the span an exact claim
        /// must be contiguous over.
        select_span = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND enqueue_seq BETWEEN ?4 AND ?5
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
        /// Delete every item of session `?1`'s batches, on session deletion.
        ///
        /// SQLite gets this from the foreign key's `ON DELETE CASCADE`; this
        /// schema's constraint is not declared cascading, so the sweep names
        /// the rows itself.
        delete_by_session = "DELETE FROM queued_work_items
             WHERE batch_id IN (
                 SELECT batch_id FROM queued_work_batches WHERE session_id = ?1
             )";
    }
}
