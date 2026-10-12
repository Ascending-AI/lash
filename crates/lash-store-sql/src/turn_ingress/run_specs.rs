//! `session_run_specs`: the run specs a session's inputs carry (FIG-3838).
//!
//! One row per `(session, spec hash)`: the canonical bytes of a non-default
//! `RunSpec`, interned once in the transaction that
//! admits the first input naming it, immutable, and owned by the session. An
//! input row names its spec through `pending_turn_inputs.run_spec_hash`; the
//! default spec is never interned.
//!
//! The bytes are read once per run, when the run resolves its shape before
//! its first phase; the run's checkpoint records the whole spec from then
//! on, and a process captures its creating run's resolved config, so
//! neither reads the row again (FIG-6021). A row is therefore reclaimable
//! once no open or running input names its hash: the session's vacuum
//! deletes it, under the session's write lock every enqueue takes, so an
//! admission that reuses a row commits its input before the vacuum reads
//! the inputs, or interns the row afresh after it. A settled input keeps
//! its hash, so its retry is still answered by its submission digest, and a
//! row is deleted with its session in any case.

/// The table's unprefixed name.
pub const TABLE: &str = "session_run_specs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, spec_hash, spec_json";

crate::statements! {
    /// `session_run_specs` statements both backends issue verbatim.
    pub struct RunSpecStatements @ "session_run_spec" {
        /// Intern spec bytes `?3` under hash `?2` for session `?1`, keeping
        /// the row an earlier input interned. The caller then reads the row
        /// back and refuses different bytes under the same hash.
        intern = "INSERT INTO session_run_specs (session_id, spec_hash, spec_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, spec_hash) DO NOTHING";

        /// The spec bytes session `?1` interned under hash `?2`.
        select_spec = "SELECT spec_json FROM session_run_specs
             WHERE session_id = ?1 AND spec_hash = ?2";

        /// Reclaim session `?1`'s specs that no unsettled input names: no
        /// run still resolves them.
        delete_unnamed = "DELETE FROM session_run_specs
             WHERE session_id = ?1
               AND spec_hash NOT IN (
                   SELECT run_spec_hash FROM pending_turn_inputs
                    WHERE session_id = ?1
                      AND run_spec_hash IS NOT NULL
                      AND {{nonterminal_turn_input_state(state)}})";

        /// Reclaim session `?1`'s specs with the session.
        delete_session = "DELETE FROM session_run_specs WHERE session_id = ?1";
    }
}
