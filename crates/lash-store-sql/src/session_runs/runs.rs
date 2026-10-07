//! `session_runs`: one row per `(session, run)` admitted work ran under,
//! holding the run's terminal evidence once it has one. The row lives until
//! its session is deleted.

/// The table's unprefixed name.
pub const TABLE: &str = "session_runs";

crate::statements! {
    /// `session_runs` statements both backends issue verbatim.
    pub struct SessionRunStatements @ "session_run" {
        /// Open run `?2` of session `?1` if it has no row yet.
        insert_open = "INSERT INTO session_runs (session_id, run) VALUES (?1, ?2)
             ON CONFLICT (session_id, run) DO NOTHING";

        /// The admission is the answer of record for this run. A retry must
        /// never widen it by looking at rows that arrived after admission.
        select_admission = "SELECT admission_json FROM session_runs
             WHERE session_id = ?1 AND run = ?2";

        /// Record the admission in the same transaction as its row bindings.
        write_admission = "UPDATE session_runs
             SET admission_json = ?3
             WHERE session_id = ?1 AND run = ?2 AND admission_json IS NULL";


        /// The one admitted run of session `?1` without terminal evidence,
        /// with its recorded admission.
        select_unfinished = "SELECT run, admission_json FROM session_runs
             WHERE session_id = ?1 AND admission_json IS NOT NULL
               AND terminal_kind IS NULL";

        /// The terminal evidence of run `?2` of session `?1`: all three
        /// columns NULL while the run has none.
        select_terminal = "SELECT terminal_cause_json, terminal_head_revision, terminal_at_ms
             FROM session_runs
             WHERE session_id = ?1 AND run = ?2";

        /// Write run `?2`'s terminal evidence (kind `?3`, cause `?4`, head
        /// revision `?5`, instant `?6`) unless it already has one: the
        /// caller decided the write against the stored evidence in the same
        /// transaction, and a zero row count means another writer won.
        write_terminal = "UPDATE session_runs
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6
             WHERE session_id = ?1 AND run = ?2 AND terminal_kind IS NULL";

        /// The runs of session `?1` without terminal evidence, in run
        /// order: what its close ends.
        select_open_runs = "SELECT run FROM session_runs
             WHERE session_id = ?1 AND terminal_kind IS NULL
             ORDER BY run";

        /// Bounded recovery page after the `(session_id, run)` cursor.
        select_open_page = "SELECT session_id, run
             FROM session_runs
             WHERE terminal_kind IS NULL
               AND (session_id > ?1 OR (session_id = ?1 AND run > ?2))
             ORDER BY session_id, run LIMIT ?3";

        /// Every run of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_runs WHERE session_id = ?1";
    }
}
