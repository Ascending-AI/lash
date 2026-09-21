//! `fork_lineage`: one row per ancestor a forked session inherits, with the
//! generation ceiling that ancestor's nodes are readable up to.

/// The table's unprefixed name.
pub const TABLE: &str = "fork_lineage";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, ancestor_session_id, fork_node_id, fork_generation";

/// One inherited ancestor and the generation it is readable up to, as the
/// readable-range statement's recursive arm projects it.
pub const READABLE_CEILING_COLUMNS: &str = "lineage.ancestor_session_id, lineage.fork_generation";

crate::statements! {
    /// `fork_lineage` statements both backends issue verbatim.
    pub struct ForkLineageStatements @ "fork_lineage" {
        /// Record that `?1` inherits `?2` up to generation `?4`, forked at
        /// node `?3`.
        insert = "INSERT INTO fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             VALUES (?1, ?2, ?3, ?4)";

        /// Drop session `?1`'s inherited ancestry at delete time.
        delete_by_session = "DELETE FROM fork_lineage WHERE session_id = ?1";
    }
}
