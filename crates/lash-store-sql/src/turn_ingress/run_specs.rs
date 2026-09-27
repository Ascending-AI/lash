//! `session_run_specs`: the run specs a session's inputs carry (FIG-3838).
//!
//! One row per `(session, spec hash)`: the canonical bytes of a non-default
//! `RunSpec`, interned once in the transaction that
//! admits the first input naming it, immutable, and owned by the session. An
//! input row names its spec through `pending_turn_inputs.run_spec_hash`; the
//! default spec is never interned. The rows are reclaimed only when their
//! session is deleted, so their growth is bounded by the number of distinct
//! specs a session ever used.

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

        /// Reclaim session `?1`'s specs with the session.
        delete_session = "DELETE FROM session_run_specs WHERE session_id = ?1";
    }
}
