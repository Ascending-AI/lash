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
        /// At most `?2` ingress obligations due at `?1`, oldest due first,
        /// each row locked for the caller's claim and skipped by every
        /// concurrent claimant: two deployments' relays take disjoint pages
        /// (ADR 0109 §1.7).
        obligation_select_due_locking = "SELECT obligation_id FROM pending_turn_inputs
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";

        /// Input `?2` of session `?1`, locked for the caller's transaction.
        select_by_id_for_update = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2 FOR UPDATE";

        /// The input session `?1` filed under source key `?2`, locked for the
        /// caller's transaction.
        select_by_source_key_for_update = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
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
                    claim_token, claim_session_lease_generation, run_spec_hash
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
                    claim_token, claim_session_lease_generation, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s unclaimed active-turn inputs, locked, which an
        /// interrupted turn's commit re-defers.
        select_pending_active = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{pending_active_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Session `?1`'s next-turn claim candidates at generation `?2`, up to
        /// `?3` of them, waiting for locked rows so the head cannot be skipped.
        ///
        /// The generation half of the predicate is the read side of the claim
        /// fence and cannot move into shared code: it is also the `ORDER BY …
        /// LIMIT` filter, so dropping it selects the wrong rows.
        claim_candidates_next_turn = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{deferred_next_turn_turn_input_state(state)}}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS commands
                    WHERE commands.session_id = ?1 AND commands.work_kind = 'control'
               )
             ORDER BY enqueue_seq ASC
             LIMIT ?3
             FOR UPDATE";

        /// [`claim_candidates_next_turn`](Self::claim_candidates_next_turn)
        /// for an admitted input root's claim (ADR 0101 §4): the root's
        /// admission chose the turn lane at a boundary whose command lane was
        /// empty, so a command enqueued since holds back only the rows after
        /// it. The prefix ends at the earliest open command; the shared
        /// session sequence orders both lanes.
        claim_candidates_admitted_root = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{deferred_next_turn_turn_input_state(state)}}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> ?2
               )
               AND NOT EXISTS (
                    SELECT 1 FROM queued_work_batches AS commands
                    WHERE commands.session_id = ?1 AND commands.work_kind = 'control'
                      AND commands.enqueue_seq < pending_turn_inputs.enqueue_seq
               )
             ORDER BY enqueue_seq ASC
             LIMIT ?3
             FOR UPDATE";

        /// Session `?1`'s claim candidates for active turn `?4` at generation
        /// `?2`, up to `?3` of them, at the `after_work` checkpoint.
        ///
        /// One statement per checkpoint because the admitted minimum-boundary
        /// set is what the checkpoint decides, and an optional predicate over a
        /// bound boundary cannot use `idx_pending_turn_inputs_session`.
        claim_candidates_active_turn_after_work = "SELECT enqueue_seq, input_id, session_id,
                    source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_token, claim_session_lease_generation, run_spec_hash
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
                    claim_token, claim_session_lease_generation, run_spec_hash
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

        /// Lock, in queue order, cancel targets `?2` of session `?1`.
        ///
        /// A cancel may write several rows, and every other multi-row writer
        /// of them locks in queue order. Taking the whole set in that order
        /// first is what keeps concurrent writers from deadlocking.
        lock_cancel_targets_in_queue_order = "SELECT enqueue_seq
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND input_id = ANY(?2::TEXT[])
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// [`lock_cancel_targets_in_queue_order`](Self::lock_cancel_targets_in_queue_order)
        /// for the suffix of session `?1` from `enqueue_seq` `?2`.
        lock_cancel_suffix_in_queue_order = "SELECT enqueue_seq
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND enqueue_seq >= ?2
             ORDER BY enqueue_seq ASC
             FOR UPDATE";

        /// Give up claim `?2`/`?3` on session `?1`, restoring each row to the
        /// open spelling its own ingress carries (FIG-1573).
        ///
        /// A row handed back to the queue owes its session a drive again: a
        /// delivered ingress obligation is due at once (ADR 0109 §3), and
        /// its next claim asks under a fresh attempt.
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
                 claim_session_lease_generation = 0,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3";

        /// The batch form of [`abandon_claim`](Self::abandon_claim), over the
        /// `(session_id, claim_id, claim_token)` triples bound as the three
        /// parallel arrays `?1`, `?2` and `?3`.
        ///
        /// One statement, not a loop: a batch abandon is one caller giving up
        /// one set of rows. The arrays keep the text fixed however many claims
        /// there are; SQLite binds one JSON array instead.
        ///
        /// A row handed back to the queue owes its session a drive again: a
        /// delivered ingress obligation is due at once (ADR 0109 §3), and
        /// its next claim asks under a fresh attempt.
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
                 claim_session_lease_generation = 0,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             FROM unnest(?1::TEXT[], ?2::TEXT[], ?3::TEXT[])
                  AS abandoned(session_id, claim_id, claim_token)
             WHERE pending_turn_inputs.session_id = abandoned.session_id
               AND pending_turn_inputs.claim_id = abandoned.claim_id
               AND pending_turn_inputs.claim_token = abandoned.claim_token";
    }
}
