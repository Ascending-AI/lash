//! A follower's snapshot: the neutral statement of the `session_follow`
//! domain (FIG-7045).
//!
//! A send's follower resolves its input or run from one statement: the
//! session ingress and run rows, the session's catalog standing, its close
//! intent and its actor's park. The park is an engine table, so this file and
//! the dialects' issuing of it are the only places the statement appears
//! (`scripts/check-durable-sql.py`); it reads and never writes, and grants no
//! authority.

crate::statements! {
    /// `session_follow` statements both backends issue verbatim.
    pub struct SessionFollowStatements @ "durable_session_follow" {
        /// Everything a follower of session `?1` decides from, in one
        /// statement and so one snapshot (FIG-7045). An input subject names
        /// input `?2` and leaves `?3` NULL; a run subject names run `?3` and
        /// leaves `?2` NULL. Columns: the input's binding, lifecycle state,
        /// whether its row is open, the session's fault and its instant,
        /// whether the host forsook the input's id; whether the session's
        /// deletion tombstone stands and whether it is materialized; whether
        /// the followed run (`?3`, else the binding) has a row, and its
        /// terminal cause, head revision, instant and kind; the session's
        /// first close intent; whether operation batch `?4` is still
        /// queued; and session actor `?5`'s park while it is parked. A claim
        /// binds the input and moves its row in one transaction, so no read
        /// here sees one without the other.
        select = "SELECT bound.admitted_run, bound.state,
             EXISTS (SELECT 1 FROM pending_turn_inputs i
                     WHERE i.session_id = ?1 AND i.input_id = ?2
                       AND {{nonterminal_turn_input_state(i.state)}}),
             meta.fault_json, meta.fault_at_ms,
             EXISTS (SELECT 1 FROM forsaken_turn_inputs f
                     WHERE f.session_id = ?1 AND f.input_id = ?2),
             EXISTS (SELECT 1 FROM deleted_sessions d WHERE d.session_id = ?1),
             EXISTS (SELECT 1 FROM session_meta m WHERE m.session_id = ?1)
               OR EXISTS (SELECT 1 FROM session_head h WHERE h.session_id = ?1),
             ran.run IS NOT NULL,
             ran.terminal_cause_json, ran.terminal_head_revision, ran.terminal_at_ms, ran.terminal_kind,
             closing.intent_id, closing.session_id, closing.format, closing.kind_json,
             closing.state_json, closing.created_at_ms,
             EXISTS (SELECT 1 FROM queued_work_batches q
                     WHERE q.session_id = ?1 AND q.batch_id = ?4 AND q.terminal_cause IS NULL),
             CASE WHEN actor.state = 'parked' THEN actor.park_json END
             FROM (SELECT 1 AS one) AS subject
             LEFT JOIN pending_turn_inputs AS bound
               ON bound.session_id = ?1 AND bound.input_id = ?2
             LEFT JOIN session_meta AS meta ON meta.session_id = ?1
             LEFT JOIN session_runs AS ran
               ON ran.session_id = ?1 AND ran.run = COALESCE(?3, bound.admitted_run)
             LEFT JOIN control_intents AS closing
               ON closing.intent_id = (SELECT MIN(c.intent_id) FROM control_intents c
                                       WHERE c.session_id = ?1 AND c.kind = 'close_session')
             LEFT JOIN actors AS actor ON actor.actor_key = ?5";
    }
}
