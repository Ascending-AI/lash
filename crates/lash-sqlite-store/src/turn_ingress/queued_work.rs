//! `queued_work_batches` statements only SQLite issues.
//!
//! The claim scan is where the two backends diverge most. PostgreSQL takes
//! `FOR UPDATE … SKIP LOCKED` over the candidate rows and reads its cutoff from
//! `transaction_timestamp()`; SQLite binds the host clock it shares with its
//! runtime and needs no row lock, because the scan already runs inside
//! `BEGIN IMMEDIATE`. The delivery-boundary rule itself is the same rule,
//! spelled twice.

lash_store_sql::statements! {
    /// `queued_work_batches` statements only SQLite issues.
    pub(crate) struct QueuedBatchSqliteStatements @ "queued_work_batch" {
        /// Enqueue batch `?1` for session `?2`, keeping an existing batch
        /// under the same source key.
        ///
        /// `enqueue_seq` is this table's `INTEGER PRIMARY KEY AUTOINCREMENT`
        /// and is never bound; PostgreSQL draws it from the column's sequence
        /// first, and returns the inserted id rather than reading it back.
        insert_new = "INSERT INTO queued_work_batches (
                 batch_id, session_id, source_key, delivery_policy, work_kind,
                 authority_json, merge_key, available_at_ms, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (session_id, source_key) DO NOTHING";

        /// The facts the settlement verdict consults about batch `?2` of
        /// session `?1`.
        ///
        /// No lock suffix: the commit already holds the database write lock.
        settlement_facts = "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM queued_work_batches
             WHERE session_id = ?1 AND batch_id = ?2";

        /// Batch `?2` of session `?1` at `?3`, if no live claim holds it.
        ///
        /// Same lock fork as [`settlement_facts`](Self::settlement_facts).
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
               ))";

        /// Cancel batch `?2` of session `?1` at `?3`, if no live claim holds
        /// it.
        ///
        /// The liveness predicate is repeated on the delete rather than
        /// inherited from the read above, because SQLite has no row lock to
        /// carry it: the write lock makes the pair atomic, and the predicate
        /// makes the delete truthful on its own. PostgreSQL's read takes
        /// `FOR UPDATE`, so its delete is keyed by id alone.
        delete_cancelled = "DELETE FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM session_execution_leases sel
                    WHERE sel.session_id = ?1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > ?3
                      AND sel.lease_fencing_token
                          = queued_work_batches.claim_session_lease_generation
               ))";

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
        ///
        /// At an idle boundary the head is whatever is ready, so the candidate
        /// set is the ready run from the head onwards. A claimed head widens
        /// the limit to the whole run because an interrupted claim must be
        /// recomposed in full.
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
             ), 0)";

        /// [`claim_candidates_idle`](Self::claim_candidates_idle) at a turn
        /// checkpoint, where only work whose delivery policy admits the
        /// earliest safe boundary may start.
        ///
        /// The head is read twice: once unfiltered, to learn the policy and
        /// claim of whatever is actually first, and once filtered, because a
        /// batch that must wait for the current turn's commit blocks
        /// everything behind it unless the head belongs to an interrupted
        /// claim this caller is recomposing.
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
                                 AND (
                                      candidate.claim_id IS NULL
                                      OR candidate.claim_id <> unfiltered.head_claim_id
                                 )
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
             ), 0)";

        /// Which of the batch ids bound as the JSON array `?2` session `?1`
        /// still holds.
        ///
        /// SQLite binds a list as a JSON array and unpacks it with `json_each`;
        /// PostgreSQL binds a real array and compares with `= ANY`.
        select_present_ids = "SELECT batch_id FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id IN (SELECT value FROM json_each(?2))";

        /// Session `?1`'s unclaimed-at-`?2` batches for generation `?3` among
        /// the ids bound as the JSON array `?4`. Same list-bind fork as
        /// [`select_present_ids`](Self::select_present_ids).
        select_by_ids = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND batch_id IN (SELECT value FROM json_each(?4))
             ORDER BY enqueue_seq ASC";

        /// The same rows keyed by the claim ids bound as the JSON array `?4`:
        /// an exact claim must validate every batch the interrupted claim it
        /// recomposes covered, not only the ones it was asked for.
        select_by_claim_ids = "SELECT enqueue_seq, batch_id, session_id, source_key,
                    delivery_policy, work_kind, authority_json, merge_key, available_at_ms,
                    enqueued_at_ms, claim_fencing_token, claim_token,
                    claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND claim_id IN (SELECT value FROM json_each(?4))
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s unclaimed-at-`?2` batches for generation `?3` whose
        /// `enqueue_seq` lies between `?4` and `?5`: the span an exact claim
        /// must be contiguous over.
        ///
        /// Same lock fork as [`settlement_facts`](Self::settlement_facts).
        select_span = "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE session_id = ?1
               AND available_at_ms <= ?2
               AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
               AND enqueue_seq BETWEEN ?4 AND ?5
             ORDER BY enqueue_seq ASC";
    }
}

lash_store_sql::statements! {
    /// `queued_work_items` statements only SQLite issues.
    pub(crate) struct QueuedItemSqliteStatements @ "queued_work_item" {
        /// The payloads of every batch id in the JSON array `?1`, keyed by
        /// batch and in item order.
        ///
        /// One page for a whole claim rather than one query per batch: the
        /// claim path hydrates a run of batches at once, and under SQLite's
        /// write lock the run cannot change between them anyway. PostgreSQL
        /// hydrates per batch inside a `REPEATABLE READ` snapshot instead, so
        /// it has no counterpart. The list bind is a JSON array unpacked with
        /// `json_each`, which is how this crate binds every list.
        list_by_batches = "SELECT batch_id, item_id, payload_json
             FROM queued_work_items
             WHERE batch_id IN (SELECT value FROM json_each(?1))
             ORDER BY batch_id ASC, item_index ASC";
    }
}
