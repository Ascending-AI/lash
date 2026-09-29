//! `fork_lineage`: one row per ancestor a forked session inherits, with the
//! generation ceiling that ancestor's nodes are readable up to.

/// The table's unprefixed name.
pub const TABLE: &str = "fork_lineage";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, ancestor_session_id, fork_node_id, fork_generation";

crate::statements! {
    /// `fork_lineage` statements both backends issue verbatim.
    pub struct ForkLineageStatements @ "fork_lineage" {
        /// Record that `?1` inherits `?2` up to generation `?4`, forked at
        /// node `?3`.
        insert = "INSERT INTO fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             VALUES (?1, ?2, ?3, ?4)";

        delete_by_session = "DELETE FROM fork_lineage WHERE session_id = ?1";

        /// Ordered pairs used for the history cursor's lineage stamp.
        select_for_stamp = "SELECT ancestor_session_id, fork_generation
             FROM fork_lineage WHERE session_id = ?1 ORDER BY ancestor_session_id";
    }
}
