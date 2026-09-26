//! `process_artifact_cleanup`: the artifact release a prune still owes.
//!
//! One row per pruned process, holding what the caller needs to release
//! the process's artifacts after its row is gone. The row is deleted when the
//! release lands, which is why the tombstone it references is `ON DELETE
//! RESTRICT`: the cleanup outlives the process and gates the tombstone's own
//! compaction.

/// The table's unprefixed name.
pub const TABLE: &str = "process_artifact_cleanup";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "process_id, cleanup_json";

crate::statements! {
    /// `process_artifact_cleanup` statements both backends issue verbatim.
    pub struct ArtifactCleanupStatements @ "process_artifact_cleanup" {
        /// Every outstanding cleanup, in process order. The caller
        /// takes the whole list: this table is a work queue that is normally
        /// empty, so paging it would cost a cursor for nothing.
        list_pending = "SELECT cleanup_json FROM process_artifact_cleanup
                 ORDER BY process_id";

        /// Acknowledge the release owed for pruned process `?1`. The caller
        /// reads the affected-row count: one is acknowledged, zero unknown.
        delete_for_process = "DELETE FROM process_artifact_cleanup
                     WHERE process_id = ?1";
    }
}
