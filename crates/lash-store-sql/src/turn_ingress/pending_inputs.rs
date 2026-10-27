//! `pending_turn_inputs`: one row per turn input submitted to a session.

/// The table's unprefixed name.
pub const TABLE: &str = "pending_turn_inputs";

/// Every column a reader decodes, in the order the row decoders expect.
///
/// Before FIG-3383 this list was hand-spelled at ten call sites across the two
/// backends and once more as a per-backend `PENDING_TURN_INPUT_COLUMNS`
/// constant. It is one list now, and a column added to it reaches every reader.
pub const COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json,
     state, input_json, enqueued_at_ms, admitted_run, admitted_by, run_spec_hash,
     terminal_at_ms, trace_cause_json, origin_json";

/// The columns written after allocation under the session's write authority.
pub const INSERT_COLUMNS: &str =
    "enqueue_seq, input_id, session_id, source_key, ingress_json, state,
     input_json, submission_digest, enqueued_at_ms, run_spec_hash, trace_cause_json, origin_json";

/// The facts source-key replay consults (FIG-3544).
///
/// Narrow for the same reason [`SETTLEMENT_COLUMNS`] is: the replay verdict
/// compares only the admission-time digest, so deciding it never decodes the
/// unbounded `input_json`; the full row is read back only on a match.
pub const REPLAY_COLUMNS: &str = "input_id, submission_digest";

/// The facts the settlement verdict
/// [`require_admitted_to_run`](lash_core::store_backend_support::require_admitted_to_run)
/// consults, and nothing else.
///
/// Narrow on purpose: this read runs once per settled input of every commit,
/// and `input_json` and `ingress_json` are unbounded caller payloads that no
/// part of the settlement decision looks at. Decoding them here would put the
/// size of a user's submission on the commit path.
pub const SETTLEMENT_COLUMNS: &str = "admitted_run, state";

crate::statements! {
    /// `pending_turn_inputs` statements both backends issue verbatim.
    ///
    /// Every write that binds a row predicates it open (`admitted_run IS
    /// NULL`), and every write that settles or releases a bound row
    /// predicates it on the run that holds it, so a row is only ever
    /// answered by the run that admitted it (FIG-3927).
    pub struct PendingInputStatements @ "pending_turn_input" {
        /// Admit input `?2` of session `?3` at sequence `?1`, allocated from
        /// the session's shared counter under the session's write authority,
        /// which the admitting transaction holds to its commit.
        ///
        /// `?5` is the submitted delivery, written once and never rewritten
        /// (ADR 0101 §5.1); `?8` is the submission digest and `?10` the
        /// interned run spec's hash, NULL for the default spec (FIG-3838). `?11`
        /// is the submission's trace cause, NULL for a root cause, written
        /// by this insert and by no later statement. `?12` is the immutable origin.
        /// Returns the admitted
        /// row as `select_by_id` reads it.
        insert_new = "INSERT INTO pending_turn_inputs (
                 enqueue_seq, input_id, session_id, source_key, ingress_json, state,
                 input_json, submission_digest, enqueued_at_ms, run_spec_hash,
                 trace_cause_json, origin_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             RETURNING enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_run, admitted_by, run_spec_hash,
                    terminal_at_ms, trace_cause_json, origin_json";

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



        /// Input `?2` of session `?1`, in any state.
        /// A settled row's `input_json` reads `NULL` once the revision its
        /// run ended on is no longer retained (FIG-6607), whether or not
        /// [`release_settled_bodies`](Self::release_settled_bodies) has
        /// cleared the bytes yet: expiry is the retained-revisions relation's,
        /// never the release backlog's.
        select_by_id = "SELECT held.enqueue_seq, held.input_id, held.session_id, held.source_key,
                    held.ingress_json, held.state,
                    CASE WHEN {{terminal_turn_input_state(held.state)}} AND EXISTS (
                        SELECT 1 FROM session_runs AS ended
                        WHERE ended.session_id = held.session_id
                          AND ended.run = held.admitted_run
                          AND ended.terminal_kind IS NOT NULL
                          AND NOT EXISTS (
                              SELECT 1 FROM session_revisions AS kept
                              WHERE kept.session_id = ended.session_id
                                AND kept.head_revision = COALESCE(
                                    ended.terminal_window_revision, ended.terminal_head_revision)
                          )
                    ) THEN NULL ELSE held.input_json END AS input_json,
                    held.enqueued_at_ms, held.admitted_run, held.admitted_by, held.run_spec_hash,
                    held.terminal_at_ms, held.trace_cause_json, held.origin_json
             FROM pending_turn_inputs AS held
             WHERE held.session_id = ?1 AND held.input_id = ?2";

        /// The lifecycle state of input `?2` of session `?1`: what a fork
        /// target naming an input no run is bound to answers from.
        select_state_by_id = "SELECT state FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// The source key input `?2` of session `?1` was accepted under: the
        /// turn a run that admits it executes (FIG-3946).
        select_source_key_by_id = "SELECT source_key FROM pending_turn_inputs
             WHERE session_id = ?1 AND input_id = ?2";

        /// The input session `?1` filed under source key `?2`, in any state.
        /// Its body reads as [`select_by_id`](Self::select_by_id)'s does.
        select_by_source_key = "SELECT held.enqueue_seq, held.input_id, held.session_id,
                    held.source_key, held.ingress_json, held.state,
                    CASE WHEN {{terminal_turn_input_state(held.state)}} AND EXISTS (
                        SELECT 1 FROM session_runs AS ended
                        WHERE ended.session_id = held.session_id
                          AND ended.run = held.admitted_run
                          AND ended.terminal_kind IS NOT NULL
                          AND NOT EXISTS (
                              SELECT 1 FROM session_revisions AS kept
                              WHERE kept.session_id = ended.session_id
                                AND kept.head_revision = COALESCE(
                                    ended.terminal_window_revision, ended.terminal_head_revision)
                          )
                    ) THEN NULL ELSE held.input_json END AS input_json,
                    held.enqueued_at_ms, held.admitted_run, held.admitted_by, held.run_spec_hash,
                    held.terminal_at_ms, held.trace_cause_json, held.origin_json
             FROM pending_turn_inputs AS held
             WHERE held.session_id = ?1 AND held.source_key = ?2";

        /// The lifecycle state and run spec of the input session `?1` filed
        /// under source key `?2`: the input that started the run a steering
        /// input addresses (FIG-3838).
        select_run_spec_by_source_key = "SELECT state, run_spec_hash, terminal_at_ms
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND source_key = ?2";

        /// The `enqueue_seq` of session `?1`'s earliest open next-turn input
        /// while `?2` is its running turn (`NULL` at idle), or `NULL`: where a
        /// composition of queued work stops, at idle and at a checkpoint
        /// alike, because the turn lane is one FIFO over both admission tables
        /// (ADR 0101 §5). Unlike the next-turn candidate scan, an open session
        /// command does not hide the input: the command lane orders nothing in
        /// the turn lane.
        ///
        /// Next-turn input is a rule over the submitted delivery, never a
        /// rewrite of it (ADR 0101 §5.1): an unaddressed row, and a row
        /// addressed to any turn but the running one. An addressed turn was
        /// running or ended when the row was admitted, so any turn but the
        /// running one has ended; at idle every open row is next-turn input.
        earliest_next_turn_candidate_seq = "SELECT MIN(enqueue_seq) FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
               AND admitted_run IS NULL
               AND ({{deferred_next_turn_turn_input_state(state)}}
                    OR ?2 IS NULL
                    OR {{ingress_turn_id(ingress_json)}} <> ?2)";

        /// Session `?1`'s undelivered inputs, open and admitted alike, with
        /// the run that holds each admitted one.
        list_undelivered = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_run, admitted_by, run_spec_hash,
                    terminal_at_ms, trace_cause_json, origin_json
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{undelivered_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s inputs a checkpoint accepted into a running run,
        /// each bound to that run until its commit settles it or its
        /// terminal releases it: the rest of what the pending read lists
        /// beside [`list_undelivered`](Self::list_undelivered) (FIG-4044).
        list_accepted = "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, admitted_run, admitted_by, run_spec_hash,
                    terminal_at_ms, trace_cause_json, origin_json
             FROM pending_turn_inputs
             WHERE session_id = ?1
               AND {{accepted_turn_input_state(state)}}
             ORDER BY enqueue_seq ASC";

        /// Session `?1`'s inputs run `?2` bound under step `?3`, in
        /// `enqueue_seq` order: what a re-executed admission step reads back
        /// instead of choosing again (FIG-3927).
        select_admitted_by_step = "SELECT enqueue_seq, input_id, session_id, source_key,
                    ingress_json, state, input_json, enqueued_at_ms, admitted_run,
                    admitted_by, run_spec_hash, terminal_at_ms, trace_cause_json, origin_json
             FROM pending_turn_inputs
             WHERE session_id = ?1 AND admitted_run = ?2 AND admitted_by = ?3
             ORDER BY enqueue_seq ASC";

        /// Withdraw open input `?2` of session `?1` into state `?3` at `?4`.
        ///
        /// Only an open row is withdrawn: a row a run admitted is that
        /// run's to settle or release, so the host's cancel changes nothing
        /// and answers the run instead.
        ///
        cancel = "UPDATE pending_turn_inputs
             SET state = ?3,
                 terminal_at_ms = ?4
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_run IS NULL";

        /// Admit open input `?2` of session `?1`, bound to run `?4` by step
        /// `?5`, at `?6`: into state `?3`, or in its own state when `?3` is
        /// `NULL`, as a next-turn admission leaves it. The submitted delivery
        /// is never rewritten (ADR 0101 §5.1).
        ///
        /// The open predicate is the write's backstop: the composition was
        /// read in the same transaction, so a row count other than one is a
        /// disagreement between the two, not a lost race.
        admit = "UPDATE pending_turn_inputs
             SET state = COALESCE(?3, state),
                 admitted_run = ?4,
                 admitted_by = ?5
             WHERE session_id = ?1
               AND input_id = ?2
               AND admitted_run IS NULL
               AND {{undelivered_turn_input_state(state)}}";

        /// Settle input `?2` of session `?1` into the terminal state `?3`
        /// under run `?4`, which must hold it, at `?5`: completed when the
        /// run delivered it, cancelled when the run drops it. The run
        /// stays on the tombstone; only the live admission step is cleared.
        /// The body stays until the revision the run ends on is released
        /// ([`release_settled_bodies`](Self::release_settled_bodies)).
        settle_admitted = "UPDATE pending_turn_inputs
             SET state = ?3,
                 terminal_at_ms = ?5,
                 admitted_by = NULL
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_run = ?4";

        /// Hand input `?2` of session `?1` back open at its own position,
        /// under run `?3`, which must hold it.
        ///
        /// An accepted row is open again in the state its submitted delivery
        /// names; the delivery itself is never rewritten (ADR 0101 §5.1), and
        /// a row addressed to a turn that is over is next-turn input by rule.
        /// A row handed back is session work again: its writer wakes the
        /// session in the same transaction.
        release_admitted = "UPDATE pending_turn_inputs
             SET state = {{released_turn_input_state(state)}},
                 admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = ?1 AND input_id = ?2 AND admitted_run = ?3";

        /// [`release_admitted`](Self::release_admitted) over every input run
        /// `?2` of session `?1` still holds: the run's terminal write, after
        /// the settlements its commit named (FIG-3927). No row stays bound to
        /// a run that has terminal evidence.
        release_run = "UPDATE pending_turn_inputs
             SET state = {{released_turn_input_state(state)}},
                 admitted_run = NULL,
                 admitted_by = NULL
             WHERE session_id = ?1 AND admitted_run = ?2 AND {{nonterminal_turn_input_state(state)}}";

        /// Reclaim session `?1`'s withdrawn inputs: cancelled before any
        /// run took them. Every other settled input keeps its submission
        /// digest and receipt until session deletion, alongside the terminal
        /// evidence of the run that took it, so a retry under its id is
        /// validated against its digest and answered from that run for the
        /// run's whole retained life (FIG-3837); only its body goes with its
        /// revision ([`release_settled_bodies`](Self::release_settled_bodies)).
        /// Each deleted input id is answered, so its attachment holds end
        /// with it.
        delete_withdrawn = "DELETE FROM pending_turn_inputs
             WHERE session_id = ?1 AND {{cancelled_turn_input_state(state)}}
               AND admitted_run IS NULL
             RETURNING input_id";

        /// Cancel input `?2` of session `?1`, bound to a run that ends
        /// unanswered, into its tombstone at `?3` with the cancelled state
        /// `?4`, letting go of any admission.
        cancel_input = "UPDATE pending_turn_inputs SET state = ?4,
            terminal_at_ms = ?3, admitted_by = NULL
            WHERE session_id = ?1 AND input_id = ?2 AND {{nonterminal_turn_input_state(state)}}";

        /// Clear the bodies of up to `?2` settled inputs of session `?1` (of
        /// every session when `?1` is NULL) whose run ended on a revision
        /// that is no longer retained (FIG-6607): the session's `Retention`
        /// released that revision, so it releases the input it settled. The
        /// row stays with its digest, its delivery and its run, so an
        /// identical retry is still answered and a changed one refused.
        ///
        /// A row whose run has not ended yet keeps its body: the revision it
        /// will end on is the head's. A withdrawn input no run took is
        /// [`delete_withdrawn`](Self::delete_withdrawn)'s. Reads treat a
        /// released row as released already ([`select_by_id`](Self::select_by_id)),
        /// so a batch left for a later release changes no answer.
        release_settled_bodies = "UPDATE pending_turn_inputs SET input_json = NULL
             WHERE (session_id, enqueue_seq) IN (
                 SELECT held.session_id, held.enqueue_seq
                 FROM pending_turn_inputs AS held
                 JOIN session_runs AS ended
                   ON ended.session_id = held.session_id AND ended.run = held.admitted_run
                 WHERE (held.session_id = ?1 OR ?1 IS NULL)
                   AND {{terminal_turn_input_state(held.state)}}
                   AND held.input_json IS NOT NULL
                   AND ended.terminal_kind IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM session_revisions AS kept
                       WHERE kept.session_id = ended.session_id
                         AND kept.head_revision = COALESCE(
                             ended.terminal_window_revision, ended.terminal_head_revision)
                   )
                 LIMIT ?2
             )";

        delete_by_session = "DELETE FROM pending_turn_inputs WHERE session_id = ?1";
    }
}
