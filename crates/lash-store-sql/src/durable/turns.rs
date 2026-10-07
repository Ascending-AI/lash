//! Turn phase state, the session commit and turn cancel requests: the neutral statements of the `turns` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name: a turn's phase state, 1:1 with its
/// `session_runs` row while the turn is unfinished. The run row stays the one
/// admission authority (its one-unfinished-run index); this row carries only
/// the phase, the encoded checkpoint and the model pin.
pub const TABLE: &str = "turn_phases";

crate::statements! {
    /// `turn_phases`, `session_runs` and `turn_cancel_requests` statements
    /// both backends issue verbatim. The session head is written by the
    /// session store's own commit, applied in the same transaction.
    pub struct TurnStatements @ "durable_turn" {
        /// Whether session `?1` has an unfinished admitted run.
        open_run = "SELECT run FROM session_runs
             WHERE session_id = ?1 AND admission_json IS NOT NULL AND terminal_kind IS NULL";

        /// Admit run `?2` of session `?1` with admission `?3`.
        insert_run = "INSERT INTO session_runs (session_id, run, admission_json)
             VALUES (?1, ?2, ?3)";

        /// Insert run `?2` of session `?1`'s phase row: phase `?3`, phase argument
        /// `?4`, iteration `?5`, checkpoint `?6`, model attempt `?7`, pinned
        /// request `?8`, model deadline `?9`, turn deadline `?10`, written at
        /// epoch `?11`.
        insert_phase = "INSERT INTO turn_phases
                 (session_id, run, phase, phase_arg, iteration, checkpoint_ref, model_attempt,
                  model_request_ref, model_deadline_ms, turn_deadline_ms, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

        /// Move run `?2` of session `?1` to phase `?3` (argument `?4`) at
        /// iteration `?5` with checkpoint `?6` and model pin `?7`, `?8`, `?9`,
        /// at epoch `?10`. No row when the run has no phase row.
        advance_phase = "UPDATE turn_phases
             SET phase = ?3, phase_arg = ?4, iteration = ?5, checkpoint_ref = ?6,
                 model_attempt = ?7, model_request_ref = ?8, model_deadline_ms = ?9,
                 written_epoch = ?10
             WHERE session_id = ?1 AND run = ?2
             RETURNING run";

        /// End unfinished run `?2` of session `?1` as `?3` with cause `?4` and
        /// head revision `?5` at `?6`. No row when it is not unfinished.
        end_run = "UPDATE session_runs
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6
             WHERE session_id = ?1 AND run = ?2
               AND admission_json IS NOT NULL AND terminal_kind IS NULL
             RETURNING run";

        /// Drop run `?2` of session `?1`'s phase row.
        delete_phase = "DELETE FROM turn_phases WHERE session_id = ?1 AND run = ?2";

        /// Session `?1`'s unfinished turn with its phase row.
        unfinished = "SELECT r.run, r.admission_json, p.phase, p.phase_arg, p.iteration,
                    p.checkpoint_ref, p.model_attempt, p.model_request_ref,
                    p.model_deadline_ms, p.turn_deadline_ms, p.written_epoch
             FROM session_runs AS r
             JOIN turn_phases AS p ON p.session_id = r.session_id AND p.run = r.run
             WHERE r.session_id = ?1 AND r.admission_json IS NOT NULL
               AND r.terminal_kind IS NULL";

        /// How run `?2` of session `?1` ended: terminal, cause and head
        /// revision. No row until it ended.
        ended = "SELECT terminal_kind, terminal_cause_json, terminal_head_revision
             FROM session_runs
             WHERE session_id = ?1 AND run = ?2 AND terminal_kind IS NOT NULL";

        /// Whether run `?2` is session `?1`'s unfinished admitted run.
        open_named_run = "SELECT run FROM session_runs
             WHERE session_id = ?1 AND run = ?2
               AND admission_json IS NOT NULL AND terminal_kind IS NULL";

        /// The cancel request run `?2` of session `?1` accepted.
        cancel_of = "SELECT request_id, origin, reason, disposition, mode
             FROM turn_cancel_requests WHERE session_id = ?1 AND turn_id = ?2";

        /// Record run `?2` of session `?1`'s first cancel request: request id
        /// `?3`, origin `?4`, reason `?5`, disposition `?6`, mode `?7`.
        insert_cancel = "INSERT INTO turn_cancel_requests
                 (session_id, turn_id, request_id, origin, reason, disposition, mode,
                  intent_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)";

        /// Escalate run `?2` of session `?1`'s accepted cancel request to mode
        /// `?3`.
        escalate_cancel = "UPDATE turn_cancel_requests
             SET mode = ?3, intent_revision = intent_revision + 1
             WHERE session_id = ?1 AND turn_id = ?2";
    }
}
