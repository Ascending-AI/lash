//! `pending_turn_inputs` statements only SQLite issues.
//!
//! Three things fork on this table. SQLite reads JSON with `json_extract` where
//! PostgreSQL casts to `jsonb` and uses `->>`; SQLite binds a list as a JSON
//! array and unpacks it with `json_each` where PostgreSQL binds a real array;
//! and SQLite takes no row lock at all, because every write path in this crate
//! already runs inside `BEGIN IMMEDIATE` and holds the database write lock for
//! the whole transaction.

lash_store_sql::statements! {
    /// `pending_turn_inputs` statements only SQLite issues.
    pub(crate) struct PendingInputSqliteStatements @ "pending_turn_input" {
        /// `enqueue_seq` is this table's `INTEGER PRIMARY KEY AUTOINCREMENT` on
        /// SQLite and is never bound; PostgreSQL draws it from the column's
        /// sequence first so its upsert path knows the value.
        ///
        /// `?4` is written to both `ingress_json` (the mutable current scope)
        /// and `submitted_ingress_json` (immutable); `?7` is the submission
        /// digest.
        insert_new = "INSERT INTO pending_turn_inputs (
                 input_id, session_id, source_key, ingress_json, state,
                 input_json, submitted_ingress_json, submission_digest, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?4, ?7, ?8)";

        /// The id and admission-time submission digest session `?1` already
        /// filed under source key `?2`.
        ///
        /// SQLite compares the digest and reads the row back only on a match,
        /// under one write lock, so the unbounded `input_json` is never read to
        /// decide a replay. PostgreSQL cannot hold "read the absence, then
        /// insert" atomic under READ COMMITTED, so it detects the conflict in
        /// the insert itself and has no counterpart to this read.
        select_id_by_source_key = "SELECT input_id, submission_digest
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// The session and immutable submission digest of the row that already
        /// holds input id `?1`, in any session: a
        /// provisioned id is unique across the store, so the enqueue that
        /// provisioned it adopts the row or refuses a foreign one.
        select_session_by_input_id = "SELECT session_id, submission_digest
             FROM pending_turn_inputs
             WHERE input_id = ?1";

        /// The facts the settlement verdict consults about input `?2` of
        /// session `?1`.
        ///
        /// No lock suffix: the commit already holds the database write lock.
        /// PostgreSQL must take the row lock explicitly.
        settlement_facts = "SELECT claim_id, claim_token, claim_session_lease_generation, state
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// Session `?1`'s inputs from `?2` onwards: the suffix a cancel
        /// anchored at one input covers. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_suffix = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC";

        /// The claim facts of session `?1`'s active-turn inputs, for the
        /// orphan scan. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_active_turn_claims = "SELECT state, ingress_json, claim_token,
                    claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s active-turn input rows, for the repair that follows
        /// the orphan scan. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_active_turn_rows = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s unclaimed active-turn inputs, which an interrupted
        /// turn's commit re-defers. Same lock fork as
        /// [`settlement_facts`](Self::settlement_facts).
        select_pending_active = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{pending_active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s next-turn claim candidates at generation `?2`, up to
        /// `?3` of them.
        ///
        /// The generation half of the predicate is the read side of the claim
        /// fence and cannot move into shared code: it is also the `ORDER BY …
        /// LIMIT` filter, so dropping it selects the wrong rows. PostgreSQL
        /// takes `FOR UPDATE SKIP LOCKED` here; SQLite is already the only
        /// writer.
        claim_candidates_next_turn = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{deferred_next_turn_turn_input_state(state)}}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
             ORDER BY enqueue_seq ASC
             LIMIT ?3";

        /// Session `?1`'s claim candidates for active turn `?4` at generation
        /// `?2`, up to `?3` of them, at the `after_work` checkpoint.
        ///
        /// One statement per checkpoint because the admitted minimum-boundary
        /// set is what the checkpoint decides, and an optional predicate over a
        /// bound boundary — `COALESCE(?N, min_boundary)` or `?N IS NULL OR …` —
        /// cannot use `idx_pending_turn_inputs_session`. The two checkpoints
        /// are picked by an exhaustive match, so a third would not compile.
        claim_candidates_active_turn_after_work = "SELECT enqueue_seq, input_id, session_id,
                    source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{active_turn_input_state(state)}}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND json_extract(ingress_json, '$.scope') = 'active_turn'
               AND json_extract(ingress_json, '$.turn_id') = ?4
               AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                   IN ('after_work')
             ORDER BY enqueue_seq ASC
             LIMIT ?3";

        /// [`claim_candidates_active_turn_after_work`](Self::claim_candidates_active_turn_after_work)
        /// at the `before_completion` checkpoint, which admits both boundaries.
        claim_candidates_active_turn_before_completion = "SELECT enqueue_seq, input_id, session_id,
                    source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{active_turn_input_state(state)}}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND json_extract(ingress_json, '$.scope') = 'active_turn'
               AND json_extract(ingress_json, '$.turn_id') = ?4
               AND COALESCE(json_extract(ingress_json, '$.min_boundary'), 'after_work')
                   IN ('after_work', 'before_completion')
             ORDER BY enqueue_seq ASC
             LIMIT ?3";

        /// Give up claim `?2`/`?3` on session `?1`, restoring each row to the
        /// open spelling its own ingress carries (FIG-1573).
        ///
        /// The restored spelling is read out of the row's `ingress_json`, so a
        /// next-turn row restores to `deferred_next_turn` and an active-turn
        /// row to `pending_active`; the JSON extraction is the fork.
        abandon_claim = "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN {{accepted_turn_input_state(state)}} THEN
                         CASE json_extract(ingress_json, '$.scope')
                             WHEN 'active_turn' THEN ?4
                             ELSE ?5
                         END
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3";

        /// The batch form of [`abandon_claim`](Self::abandon_claim), over the
        /// `(session_id, claim_id, claim_token)` triples bound as the JSON
        /// array `?1`.
        ///
        /// One statement, not a loop: a batch abandon is one caller giving up
        /// one set of rows. The row-value `IN (SELECT …)` keeps
        /// `idx_pending_turn_inputs_claim` seekable —
        /// `abandoning_a_batch_of_claims_seeks_the_claim_index` pins that —
        /// where a correlated `EXISTS` would scan. PostgreSQL binds three real
        /// arrays and joins `unnest` instead.
        abandon_claims = "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN {{accepted_turn_input_state(state)}} THEN
                         CASE json_extract(ingress_json, '$.scope')
                             WHEN 'active_turn' THEN ?2
                             ELSE ?3
                         END
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN (
                 SELECT json_extract(abandoned.value, '$[0]'),
                        json_extract(abandoned.value, '$[1]'),
                        json_extract(abandoned.value, '$[2]')
                 FROM json_each(?1) AS abandoned
             )";
    }
}
