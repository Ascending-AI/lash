//! `session_roots`: one row per `(session, root)` a drive admitted work
//! under, holding the root's terminal evidence once it has one. The row lives
//! until its session is deleted.

/// The table's unprefixed name.
pub const TABLE: &str = "session_roots";

/// The terminal evidence columns, in the order both backends decode them.
pub const TERMINAL_COLUMNS: &str =
    "terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms";

/// The key columns alone: opening a root writes its identity and nothing
/// else, so its terminal columns stay NULL until an end writes them.
pub const KEY_COLUMNS: &str = "session_id, root";

crate::statements! {
    /// `session_roots` statements both backends issue verbatim.
    pub struct SessionRootStatements @ "session_root" {
        /// Open root `?2` of session `?1` if it has no row yet.
        insert_open = "INSERT INTO session_roots (session_id, root) VALUES (?1, ?2)
             ON CONFLICT (session_id, root) DO NOTHING";

        /// The terminal evidence of root `?2` of session `?1`: all four
        /// columns NULL while the root has none.
        select_terminal = "SELECT terminal_kind, terminal_cause_json, terminal_head_revision, terminal_at_ms
             FROM session_roots
             WHERE session_id = ?1 AND root = ?2";

        /// Write root `?2`'s terminal evidence (kind `?3`, cause `?4`, head
        /// revision `?5`, instant `?6`) unless it already has one: the
        /// caller decided the write against the stored evidence in the same
        /// transaction, and a zero row count means another writer won.
        write_terminal = "UPDATE session_roots
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6
             WHERE session_id = ?1 AND root = ?2 AND terminal_kind IS NULL";

        /// Every root of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_roots WHERE session_id = ?1";
    }
}
