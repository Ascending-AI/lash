//! `session_runs`: one row per `(session, run)` admitted work ran under,
//! holding the run's terminal evidence once it has one. The row lives until
//! its session is deleted.
//!
//! A terminal names the session window it ended on by the head revision it
//! ended on, nothing more (FIG-5931, FIG-6606). The window is that
//! revision's row in `session_revisions`: its leaf, checkpoint and head
//! document. The session's retention releases it with the revision, a pin
//! on the revision keeps it exact, and once it is released a read of it is
//! refused as released; the compact terminal evidence stays readable.

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

        /// The one admitted run of session `?1` without terminal evidence,
        /// with its recorded admission.
        select_unfinished = "SELECT run, admission_json FROM session_runs
             WHERE session_id = ?1 AND admission_json IS NOT NULL
               AND terminal_kind IS NULL";

        /// The terminal evidence of run `?2` of session `?1`: every
        /// column NULL while the run has none. The decoder checks the
        /// indexed kind against the decoded cause.
        select_terminal = "SELECT terminal_cause_json, terminal_head_revision, terminal_at_ms, terminal_kind
             FROM session_runs
             WHERE session_id = ?1 AND run = ?2";

        /// Write run `?2`'s terminal evidence (kind `?3`, cause `?4`, head
        /// revision `?5`, instant `?6` and window revision `?7`) unless it
        /// already has one: the caller decided the write against the stored
        /// evidence in the same transaction, and a zero row count means
        /// another writer won.
        write_terminal = "UPDATE session_runs
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6,
                 terminal_window_revision = ?7
             WHERE session_id = ?1 AND run = ?2 AND terminal_kind IS NULL";

        /// Session `?1`'s published head revision, when its revision row
        /// stands: what a terminal's window names.
        select_window_head = "SELECT revision.head_revision
             FROM session_head AS head
             JOIN session_revisions AS revision
               ON revision.session_id = head.session_id
              AND revision.head_revision = head.head_revision
             WHERE head.session_id = ?1";

        /// The window run `?2` of session `?1` ended on, in the columns
        /// order of the head's own read (`select_meta`): its config graph
        /// root, revision, leaf, checkpoint and the frame its leaf row names,
        /// then the revision row's own revision, NULL once the session's
        /// retention released it. No row when the run has no terminal or its
        /// terminal names no window.
        select_terminal_window = "SELECT revision.config_ref, ended.terminal_window_revision,
                    revision.leaf_node_id, revision.checkpoint_ref, leaf.frame_node_id,
                    revision.head_revision
             FROM session_runs AS ended
             LEFT JOIN session_revisions AS revision
               ON revision.session_id = ended.session_id
              AND revision.head_revision = ended.terminal_window_revision
             LEFT JOIN graph_nodes AS leaf ON leaf.node_id = revision.leaf_node_id
             WHERE ended.session_id = ?1 AND ended.run = ?2
               AND ended.terminal_kind IS NOT NULL AND ended.terminal_window_revision IS NOT NULL";

        /// The runs of session `?1` without terminal evidence, in run
        /// order: what its close ends.
        select_open_runs = "SELECT run FROM session_runs
             WHERE session_id = ?1 AND terminal_kind IS NULL
             ORDER BY run";

        /// Every run of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_runs WHERE session_id = ?1";

    }
}
