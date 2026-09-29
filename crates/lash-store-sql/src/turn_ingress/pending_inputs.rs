//! `pending_turn_inputs`: one row per turn input submitted to a session.

/// The table's unprefixed name.
pub const TABLE: &str = "pending_turn_inputs";

/// Every column a reader decodes, in the order the row decoders expect.
///
/// Before FIG-3383 this list was hand-spelled at ten call sites across the two
/// backends and once more as a per-backend `PENDING_TURN_INPUT_COLUMNS`
/// constant. It is one list now, and a column added to it reaches every reader.
pub const COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json,
     state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash";

/// The columns written after allocation under the session's write authority.
pub const INSERT_COLUMNS: &str =
    "enqueue_seq, input_id, session_id, source_key, ingress_json, state,
     input_json, submitted_ingress_json, submission_digest, enqueued_at_ms, run_spec_hash";

/// The facts source-key replay consults (FIG-3544).
///
/// Narrow for the same reason [`SETTLEMENT_COLUMNS`] is: the replay verdict
/// compares only the admission-time digest, so deciding it never decodes the
/// unbounded `input_json`; the full row is read back only on a match.
pub const REPLAY_COLUMNS: &str = "input_id, submission_digest";

/// The facts the settlement verdict
/// [`require_admitted_to_root`](lash_core::store_backend_support::require_admitted_to_root)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled input of every commit,
/// and `input_json` and `ingress_json` are unbounded caller payloads that no
/// part of the settlement decision looks at. Decoding them here would put the
/// size of a user's submission on the commit path.
pub const SETTLEMENT_COLUMNS: &str = "admitted_root, state";

crate::statements! {
    /// `pending_turn_inputs` statements both backends issue verbatim.
    ///
    /// Every write that binds a row predicates it open (`admitted_root IS
    /// NULL`), and every write that settles or releases a bound row
    /// predicates it on the root that holds it, so a row is only ever
    /// answered by the root that admitted it (FIG-3927).
    pub struct PendingInputStatements @ "pending_turn_input" {
        /// Admit input `?2` of session `?3` at sequence `?1`, allocated from
        /// the session's shared counter under the session's write authority,
        /// which the admitting transaction holds to its commit.
        ///
        /// `?5` is written to both `ingress_json` (the mutable current scope)
        /// and `submitted_ingress_json` (immutable); `?8` is the submission
        /// digest and `?10` the interned run spec's hash, NULL for the default
        /// spec (FIG-3838).
        insert_new = "INSERT INTO pending_turn_inputs (
                 enqueue_seq, input_id, session_id, source_key, ingress_json, state,
                 input_json, submitted_ingress_json, submission_digest, enqueued_at_ms,
                 run_spec_hash
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?5, ?8, ?9, ?10)";

        /// The id and admission-time submission digest session `?1` already
        /// filed under source key `?2`.
        ///
        /// Read under the session's write authority, which every admission
        /// takes before it reads, so the absence it answers holds until the
        /// admitting transaction commits. The verdict compares the digest and
        /// reads the row back only on a match, so the unbounded `input_json` is
        /// never read to decide a replay (FIG-3544).
        select_id_by_source_key = "SELECT input_id, submission_digest
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// The session and immutable submission digest of the row that already
        /// holds input id `?1`, in any session: a provisioned id is unique
        /// across the store, so the enqueue that provisioned it adopts the row
        /// or refuses a foreign one (FIG-3513). Another session's concurrent
        /// admission of the same id is not serialized by this session's
        /// authority; the id's unique constraint refuses the second insert.
        select_session_by_input_id = "SELECT session_id, submission_digest
             FROM pending_turn_inputs
             WHERE input_id = ?1";

        select_by_id = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// The input session `?1` filed under source key `?2`.
        select_by_source_key = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// The lifecycle state and run spec of the input session `?1` filed
        /// under source key `?2`: the input that started the root a steering
        /// input addresses (FIG-3838).
        select_run_spec_by_source_key = "SELECT state, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// The `enqueue_seq` of session `?1`'s earliest open next-turn input,
        /// or `NULL`: where a composition of queued work stops, at idle and
        /// at a checkpoint alike, because the turn lane is one FIFO over both
        /// admission tables (ADR 0101 §5). Unlike the next-turn candidate
        /// scan, an open session command does not hide the input: the command
        /// lane orders nothing in the turn lane.
        earliest_next_turn_candidate_seq = "SELECT MIN(enqueue_seq) FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_root IS NULL
               AND {{deferred_next_turn_turn_input_state(state)}}";

        /// Session `?1`'s undelivered inputs, open and admitted alike, with
        /// the root that holds each admitted one.
        list_undelivered = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s inputs a checkpoint accepted into a running root,
        /// each bound to that root until its commit settles it or its
        /// terminal releases it: the rest of what the pending read lists
        /// beside [`list_undelivered`](Self::list_undelivered) (FIG-4044).
        list_accepted = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_root, admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{accepted_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s inputs root `?2` bound under step `?3`, in
        /// `enqueue_seq` order: what a re-executed admission step reads back
        /// instead of choosing again (FIG-3927).
        select_admitted_by_step = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_root,
                    admitted_by, run_spec_hash
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_root = ?2 AND admitted_by = ?3
             ORDER BY enqueue_seq ASC";

        /// Withdraw open input `?2` of session `?1` into state `?3` at `?4`.
        ///
        /// Only an open row is withdrawn: a row a root admitted is that
        /// root's to settle or release, so the host's cancel changes nothing
        /// and answers the root instead.
        ///
        /// A withdrawn row owes its session no drive, and nothing admits it
        /// after, so the withdrawal settles the row's ingress obligation in
        /// the same write (ADR 0109 §3, FIG-4098): due, claimed by a relay
        /// that asked for a drive, or stalled, it is delivered now, as an
        /// admission would deliver it.
        cancel = "UPDATE pending_turn_inputs
             SET state = ?3,
                 obligation_state = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN 'delivered' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_due_at_ms END,
                 obligation_claim_token = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_claim_token END,
                 obligation_stall_reason = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_stall_reason END,
                 obligation_settled_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN ?4 ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_root IS NULL";

        /// Re-defer open input `?2` of session `?1` to state `?3` under the
        /// next-turn ingress `?4`: the active-turn input an interrupted turn
        /// never admitted.
        ///
        /// The ingress is rewritten, not preserved: a row pinned to a turn
        /// that is over must stop naming it, or the next admission pins it
        /// to the same dead turn (FIG-1573).
        ///
        /// Only the mutable `ingress_json` moves. `submitted_ingress_json` and
        /// `submission_digest` are written once at admission and never
        /// updated, so an identical source-key retry still matches after
        /// this rewrite (FIG-3544).
        defer_to_next_turn = "UPDATE pending_turn_inputs
             SET state = ?3,
                 ingress_json = ?4
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_root IS NULL";

        /// Admit open input `?2` of session `?1` into state `?3`, bound to
        /// root `?4` by step `?5`, at `?6`.
        ///
        /// The admission is the row's delivery, so it delivers the row's
        /// ingress obligation in the same write (ADR 0109 §3): due, claimed
        /// by a relay that asked for a drive, or stalled, it is delivered now.
        ///
        /// The open predicate is the write's backstop: the composition was
        /// read in the same transaction, so a row count other than one is a
        /// disagreement between the two, not a lost race.
        admit = "UPDATE pending_turn_inputs
             SET state = ?3,
                 admitted_root = ?4,
                 admitted_by = ?5,
                 obligation_state = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN 'delivered' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_due_at_ms END,
                 obligation_claim_token = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_claim_token END,
                 obligation_stall_reason = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN NULL ELSE obligation_stall_reason END,
                 obligation_settled_at_ms = CASE WHEN obligation_state IN ('due', 'claimed', 'stalled')
                     THEN ?6 ELSE obligation_settled_at_ms END
             WHERE session_id = ?1
               AND input_id = ?2
               AND admitted_root IS NULL
               AND {{undelivered_turn_input_state(state)}}";

        /// Settle input `?2` of session `?1` into the terminal state `?3`
        /// under root `?4`, which must hold it: completed when the root
        /// delivered it, cancelled when the root drops it. The binding goes
        /// with the settlement.
        settle_admitted = "UPDATE pending_turn_inputs
             SET state = ?3,
                 admitted_root = NULL,
                 admitted_by = NULL
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_root = ?4";

        /// Hand input `?2` of session `?1` back open at its own position,
        /// under root `?3`, which must hold it.
        ///
        /// An active-turn row names a turn that is over, so it is re-deferred
        /// to the next turn in state `?4` under the next-turn ingress `?5`
        /// (FIG-1573). A row handed back owes its session a drive again: a
        /// delivered ingress obligation is due at once (ADR 0109 §3).
        release_admitted = "UPDATE pending_turn_inputs
             SET state = CASE WHEN {{active_turn_input_state(state)}} THEN ?4 ELSE state END,
                 ingress_json = CASE WHEN {{active_turn_input_state(state)}}
                     THEN ?5 ELSE ingress_json END,
                 admitted_root = NULL,
                 admitted_by = NULL,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_root = ?3";

        /// [`release_admitted`](Self::release_admitted) over every input root
        /// `?2` of session `?1` still holds, with `?3`/`?4` the next-turn
        /// state and ingress: the root's terminal write, after the
        /// settlements its commit named (FIG-3927). No row stays bound to a
        /// root that has terminal evidence.
        release_root = "UPDATE pending_turn_inputs
             SET state = CASE WHEN {{active_turn_input_state(state)}} THEN ?3 ELSE state END,
                 ingress_json = CASE WHEN {{active_turn_input_state(state)}}
                     THEN ?4 ELSE ingress_json END,
                 admitted_root = NULL,
                 admitted_by = NULL,
                 obligation_state = CASE WHEN obligation_state = 'delivered'
                     THEN 'due' ELSE obligation_state END,
                 obligation_due_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN 0 ELSE obligation_due_at_ms END,
                 obligation_settled_at_ms = CASE WHEN obligation_state = 'delivered'
                     THEN NULL ELSE obligation_settled_at_ms END
             WHERE session_id = ?1 AND admitted_root = ?2";

        /// Reclaim session `?1`'s withdrawn inputs: cancelled before any
        /// root took them. Every other settled input keeps its submission
        /// digest and receipt until session deletion, alongside the terminal
        /// evidence of the root that took it, so a retry under its id is
        /// validated against its digest and answered from that root for the
        /// root's whole retained life (FIG-3837).
        delete_withdrawn = "DELETE FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{cancelled_turn_input_state(state)}}
               AND NOT EXISTS (
                 SELECT 1 FROM session_root_inputs binding
                 WHERE binding.session_id = pending_turn_inputs.session_id
                   AND binding.input_id = pending_turn_inputs.input_id
               )";

        delete_by_session = "DELETE FROM pending_turn_inputs WHERE session_id = ?1";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct PendingRootVerbStatements @ "pending_turn_input" {
        /// Move input `?2` of session `?1`, bound to a root a verb ends, into
        /// state `?3`, letting go of any admission: cancelled by a cancel,
        /// re-deferred by a fork.
        input = "UPDATE pending_turn_inputs SET state = ?3,
            admitted_root = NULL, admitted_by = NULL
            WHERE session_id = ?1 AND input_id = ?2 AND {{nonterminal_turn_input_state(state)}}";
    }
}

/// The obligation columns a claim reads back (ADR 0109 §1.3): the id, the
/// attempt count after the claim, then the row's key.
pub const OBLIGATION_CLAIM_COLUMNS: &str =
    "obligation_id, obligation_attempts, session_id, input_id";

crate::statements! {
    /// `pending_turn_inputs` obligation statements (ADR 0109): an admitted
    /// input owes its session a drive. Both backends issue them verbatim;
    /// every settling write compares the state and, while claimed, the claim
    /// token.
    pub struct PendingTurnInputObligationStatements @ "pending_turn_input" {
        /// Arm the row keyed `?1`, `?2` as obligation `?3`, due at `?4`, if
        /// it owes nothing.
        obligation_arm = "UPDATE pending_turn_inputs
             SET obligation_id = ?3, obligation_state = 'due', obligation_attempts = 0,
                 obligation_due_at_ms = ?4, obligation_claim_token = NULL,
                 obligation_stall_reason = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = NULL
             WHERE session_id = ?1 AND input_id = ?2 AND obligation_state IS NULL";

        /// At most `?2` obligations due at `?1`, a lapsed claim included,
        /// oldest due first.
        obligation_select_due = "SELECT obligation_id FROM pending_turn_inputs
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// The due instant and id of at most `?2` obligations due at `?1`,
        /// oldest due first: what the ingress ledger merges across its two
        /// tables before it claims either.
        obligation_peek_due = "SELECT obligation_due_at_ms, obligation_id FROM pending_turn_inputs
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2";

        /// Claim obligation `?1` under token `?2` until `?3` if it is still
        /// due at `?4`.
        obligation_claim_due_row = "UPDATE pending_turn_inputs
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state IN ('due', 'claimed')
               AND obligation_due_at_ms <= ?4
             RETURNING obligation_id, obligation_attempts, session_id, input_id";

        /// Claim `due` obligation `?1` under token `?2` until `?3`, whatever
        /// its backoff: a producer's own immediate attempt.
        obligation_claim = "UPDATE pending_turn_inputs
             SET obligation_state = 'claimed', obligation_claim_token = ?2,
                 obligation_attempts = obligation_attempts + 1, obligation_due_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'due'
             RETURNING obligation_id, obligation_attempts, session_id, input_id";

        /// Settle claim `?2` on obligation `?1` delivered at `?3`.
        obligation_settle_delivered = "UPDATE pending_turn_inputs
             SET obligation_state = 'delivered', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_last_error = NULL,
                 obligation_settled_at_ms = ?3
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Hand claim `?2` on obligation `?1` back, due again at `?3`, with
        /// error `?4`.
        obligation_settle_retry = "UPDATE pending_turn_inputs
             SET obligation_state = 'due', obligation_claim_token = NULL,
                 obligation_due_at_ms = ?3, obligation_last_error = ?4
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Stall claim `?2` on obligation `?1` for reason `?3` with error
        /// `?4` at `?5`.
        obligation_settle_stall = "UPDATE pending_turn_inputs
             SET obligation_state = 'stalled', obligation_claim_token = NULL,
                 obligation_due_at_ms = NULL, obligation_stall_reason = ?3,
                 obligation_last_error = ?4, obligation_settled_at_ms = ?5
             WHERE obligation_id = ?1 AND obligation_state = 'claimed'
               AND obligation_claim_token = ?2";

        /// Re-arm stalled obligation `?1`, due at `?2`, its attempts reset.
        obligation_rearm = "UPDATE pending_turn_inputs
             SET obligation_state = 'due', obligation_attempts = 0, obligation_due_at_ms = ?2,
                 obligation_stall_reason = NULL, obligation_settled_at_ms = NULL
             WHERE obligation_id = ?1 AND obligation_state = 'stalled'";

        /// At most `?2` stalled obligations after id `?1`, in id order.
        obligation_select_stalled = "SELECT obligation_id, obligation_attempts, obligation_stall_reason, obligation_last_error, obligation_settled_at_ms, session_id, input_id
             FROM pending_turn_inputs
             WHERE obligation_state = 'stalled' AND obligation_id > ?1
             ORDER BY obligation_id
             LIMIT ?2";

        /// How many obligations are stalled.
        obligation_count_stalled = "SELECT COUNT(*) FROM pending_turn_inputs WHERE obligation_state = 'stalled'";

        /// Obligation `?1`'s state and the claims taken since it was armed.
        obligation_select_standing = "SELECT obligation_state, obligation_attempts FROM pending_turn_inputs WHERE obligation_id = ?1";
    }
}

impl crate::obligation::ObligationStatementSet for PendingTurnInputObligationStatements {
    fn obligation_sql(&self) -> crate::obligation::ObligationSql<'_> {
        crate::obligation::ObligationSql {
            key_columns: 2,
            arm: &self.obligation_arm,
            select_due: &self.obligation_select_due,
            claim_due_row: &self.obligation_claim_due_row,
            claim: &self.obligation_claim,
            settle_delivered: &self.obligation_settle_delivered,
            settle_retry: &self.obligation_settle_retry,
            settle_stall: &self.obligation_settle_stall,
            rearm: &self.obligation_rearm,
            select_stalled: &self.obligation_select_stalled,
            count_stalled: &self.obligation_count_stalled,
            select_standing: &self.obligation_select_standing,
        }
    }
}
