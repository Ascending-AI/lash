//! `session_runs`: one row per `(session, run)` admitted work ran under,
//! holding the run's terminal evidence once it has one. The row lives until
//! its session is deleted.
//!
//! A terminal names the session window it ended on by reference: the head
//! revision, leaf and checkpoint the head published, and the head document
//! in `blobs`, stored once per distinct document of a
//! session (FIG-5931). The leaf and the checkpoint are roots while the row
//! lives, so the window reads the same after any collection.

/// The table's unprefixed name.
pub const TABLE: &str = "session_runs";

/// The published head a terminal window retains: revision, leaf, checkpoint
/// and head-document digest. Its leaf and checkpoint remain roots for the
/// lifetime of the terminal row, even after later commits or collection.
pub struct TerminalWindowRef {
    pub revision: i64,
    pub leaf: Option<String>,
    pub checkpoint: Option<String>,
    pub config_ref: String,
}

impl TerminalWindowRef {
    /// The `write_terminal` parameters ?7 to ?10; an absent head names no window.
    pub fn columns(
        window: Option<&Self>,
    ) -> (Option<i64>, Option<&str>, Option<&str>, Option<&str>) {
        window.map_or((None, None, None, None), |window| {
            (
                Some(window.revision),
                window.leaf.as_deref(),
                window.checkpoint.as_deref(),
                Some(window.config_ref.as_str()),
            )
        })
    }
}

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
        /// revision `?5`, instant `?6`, window revision `?7`, leaf `?8`,
        /// checkpoint `?9` and head document digest `?10`) unless it already
        /// has one: the caller decided the write against the stored evidence
        /// in the same transaction, and a zero row count means another
        /// writer won.
        write_terminal = "UPDATE session_runs
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6,
                 terminal_window_revision = ?7, terminal_window_leaf_node_id = ?8,
                 terminal_window_checkpoint_ref = ?9, terminal_window_config_ref = ?10
             WHERE session_id = ?1 AND run = ?2 AND terminal_kind IS NULL";

        /// Session `?1`'s published head: its revision, leaf, checkpoint and
        /// head document, what a terminal's window names.
        select_window_head = "SELECT revision.head_revision, revision.leaf_node_id,
                    revision.checkpoint_ref, revision.config_ref
             FROM session_head AS head
             JOIN session_revisions AS revision
               ON revision.session_id = head.session_id
              AND revision.head_revision = head.head_revision
             WHERE head.session_id = ?1";

        /// The window run `?2` of session `?1` ended on, in the columns
        /// order of the head's own read (`select_meta`): its
        /// head document, revision, leaf, checkpoint and the frame its leaf
        /// row names. No row when the run has no terminal or its terminal
        /// names no window.
        select_terminal_window = "SELECT ended.terminal_window_config_ref, ended.terminal_window_revision,
                    ended.terminal_window_leaf_node_id,
                    ended.terminal_window_checkpoint_ref, leaf.frame_node_id
             FROM session_runs AS ended
             LEFT JOIN graph_nodes AS leaf
               ON leaf.node_id = ended.terminal_window_leaf_node_id
             WHERE ended.session_id = ?1 AND ended.run = ?2
               AND ended.terminal_kind IS NOT NULL AND ended.terminal_window_revision IS NOT NULL";

        /// The distinct checkpoints session `?1`'s terminal windows root,
        /// in hash order: what its deletion may reclaim.
        select_session_window_checkpoints = "SELECT DISTINCT terminal_window_checkpoint_ref
             FROM session_runs
             WHERE session_id = ?1 AND terminal_window_checkpoint_ref IS NOT NULL
             UNION SELECT terminal_window_config_ref FROM session_runs
                 WHERE session_id = ?1 AND terminal_window_config_ref IS NOT NULL
             ORDER BY terminal_window_checkpoint_ref";

        /// The distinct leaves session `?1`'s terminal windows retain: where
        /// its deletion starts their ancestry's retirement.
        select_session_window_leaves = "SELECT DISTINCT terminal_window_leaf_node_id
             FROM session_runs
             WHERE session_id = ?1 AND terminal_window_leaf_node_id IS NOT NULL
             ORDER BY terminal_window_leaf_node_id";

        /// The runs of session `?1` without terminal evidence, in run
        /// order: what its close ends.
        select_open_runs = "SELECT run FROM session_runs
             WHERE session_id = ?1 AND terminal_kind IS NULL
             ORDER BY run";

        /// Every run of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_runs WHERE session_id = ?1";

    }
}
