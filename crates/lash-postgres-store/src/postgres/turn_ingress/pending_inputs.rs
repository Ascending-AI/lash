//! `pending_turn_inputs` statements only PostgreSQL issues.
//!
//! Three things fork on this table. PostgreSQL reads JSON by casting to `jsonb`
//! and using `->>` where SQLite calls `json_extract`; it binds a list as a real
//! array where SQLite binds a JSON array; and it must take row locks
//! explicitly, because check-then-act on a row is not atomic under READ
//! COMMITTED and SQLite already holds the database write lock.

lash_store_sql::statements! {
    /// `pending_turn_inputs` statements only PostgreSQL issues.
    pub(crate) struct PendingInputPostgresStatements @ "pending_turn_input" {
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
                 'lash_pending_turn_inputs',
                 'enqueue_seq'
             ))";

        /// PostgreSQL draws `enqueue_seq` from the column's sequence before the
        /// insert so the source-key path knows the value it is about to write;
        /// SQLite lets the row allocate its own.
        ///
        /// `?5` is written to both `ingress_json` (the mutable current scope)
        /// and `submitted_ingress_json` (immutable); `?9` is the submission
        /// digest.
        insert_new = "INSERT INTO pending_turn_inputs (
                 enqueue_seq, input_id, session_id, source_key, ingress_json, state,
                 input_json, submitted_ingress_json, submission_digest, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?5, ?9, ?8)";

        /// Enqueue input `?2`, or hand back the row session `?3` already filed
        /// under the same source key.
        ///
        /// PostgreSQL cannot hold "read the absence, then insert" atomic under
        /// READ COMMITTED, so the conflict clause is what detects a concurrent
        /// submitter, and the no-op `DO UPDATE` is what makes `RETURNING` hand
        /// back the existing row rather than nothing. SQLite reads the absence
        /// under the same write lock it inserts under and has no counterpart.
        ///
        /// `RETURNING` appends the row's admission-time `submission_digest`
        /// after the decoder's columns: the caller compares it against the
        /// draft's digest, never the row's mutable current ingress (FIG-3544).
        insert_or_adopt_existing = "INSERT INTO pending_turn_inputs (
                 enqueue_seq, input_id, session_id, source_key, ingress_json, state,
                 input_json, submitted_ingress_json, submission_digest, enqueued_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?5, ?9, ?8)
             ON CONFLICT (session_id, source_key) DO UPDATE
                 SET source_key = pending_turn_inputs.source_key
             RETURNING enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, submission_digest";

        /// Input `?2` of session `?1`, locked for the caller's transaction.
        select_by_id_for_update = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2 FOR UPDATE";

        /// The input session `?1` filed under source key `?2`, locked for the
        /// caller's transaction.
        select_by_source_key_for_update = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2 FOR UPDATE";

        /// The facts the settlement verdict consults about input `?2` of
        /// session `?1`, locked for the caller's transaction.
        settlement_facts = "SELECT claim_id, claim_token, claim_session_lease_generation, state
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2 LIMIT 1 FOR UPDATE";

        /// Session `?1`'s inputs from `?2` onwards, locked: the suffix a cancel
        /// anchored at one input covers.
        select_suffix = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// The claim facts of session `?1`'s active-turn inputs, locked, for
        /// the orphan scan.
        select_active_turn_claims = "SELECT state, ingress_json, claim_token,
                    claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s active-turn input rows, locked, for the repair that
        /// follows the orphan scan.
        select_active_turn_rows = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s unclaimed active-turn inputs, locked, which an
        /// interrupted turn's commit re-defers.
        select_pending_active = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{pending_active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s next-turn claim candidates at generation `?2`, up to
        /// `?3` of them, locked and skipping rows another claimant holds.
        ///
        /// The generation half of the predicate is the read side of the claim
        /// fence and cannot move into shared code: it is also the `ORDER BY …
        /// LIMIT` filter, so dropping it selects the wrong rows.
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
             LIMIT ?3
             FOR UPDATE SKIP LOCKED";

        /// Session `?1`'s claim candidates for active turn `?4` at generation
        /// `?2`, up to `?3` of them, at the `after_work` checkpoint.
        ///
        /// One statement per checkpoint because the admitted minimum-boundary
        /// set is what the checkpoint decides, and an optional predicate over a
        /// bound boundary cannot use `idx_pending_turn_inputs_session`.
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
               AND ingress_json::jsonb ->> 'scope' = 'active_turn'
               AND ingress_json::jsonb ->> 'turn_id' = ?4
               AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                   IN ('after_work')
             ORDER BY enqueue_seq ASC
             LIMIT ?3
             FOR UPDATE SKIP LOCKED";

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
               AND ingress_json::jsonb ->> 'scope' = 'active_turn'
               AND ingress_json::jsonb ->> 'turn_id' = ?4
               AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work')
                   IN ('after_work', 'before_completion')
             ORDER BY enqueue_seq ASC
             LIMIT ?3
             FOR UPDATE SKIP LOCKED";

        /// Give up claim `?2`/`?3` on session `?1`, restoring each row to the
        /// open spelling its own ingress carries (FIG-1573).
        abandon_claim = "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN {{accepted_turn_input_state(state)}} THEN
                         CASE ingress_json::jsonb ->> 'scope'
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
        /// `(session_id, claim_id, claim_token)` triples bound as the three
        /// parallel arrays `?1`, `?2` and `?3`.
        ///
        /// One statement, not a loop: a batch abandon is one caller giving up
        /// one set of rows. The arrays keep the text fixed however many claims
        /// there are; SQLite binds one JSON array instead.
        abandon_claims = "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN {{accepted_turn_input_state(state)}} THEN
                         CASE ingress_json::jsonb ->> 'scope'
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
             FROM unnest(?1::TEXT[], ?2::TEXT[], ?3::TEXT[])
                  AS abandoned(session_id, claim_id, claim_token)
             WHERE pending_turn_inputs.session_id = abandoned.session_id
               AND pending_turn_inputs.claim_id = abandoned.claim_id
               AND pending_turn_inputs.claim_token = abandoned.claim_token";
    }
}
