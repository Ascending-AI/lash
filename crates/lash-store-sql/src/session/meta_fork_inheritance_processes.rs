//! `session_meta_fork_inheritance_processes`: the ordered processes a forked
//! session inherits, one row per process.

/// The table's unprefixed name.
pub const TABLE: &str = "session_meta_fork_inheritance_processes";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, process_index, process_id";

/// One inherited process as the metadata load reads it back: the full row
/// minus the `session_id` the read is keyed by.
pub const PROCESS_COLUMNS: &str = "process_index, process_id";

crate::statements! {
    /// `session_meta_fork_inheritance_processes` statements both backends
    /// issue verbatim.
    pub struct ForkInheritanceStatements @ "session_meta_fork_inheritance" {
        /// Session `?1`'s inherited processes, in index order.
        select_for_session = "SELECT process_index, process_id FROM session_meta_fork_inheritance_processes
             WHERE session_id = ?1 ORDER BY process_index";

        /// Record inherited process `?2` of session `?1`.
        insert = "INSERT INTO session_meta_fork_inheritance_processes (session_id, process_index, process_id) VALUES (?1, ?2, ?3)";

        /// Drop session `?1`'s inherited processes, before a metadata write
        /// rewrites them and at delete time.
        delete_by_session =
            "DELETE FROM session_meta_fork_inheritance_processes WHERE session_id = ?1";
    }
}
