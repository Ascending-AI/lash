//! `graph_nodes`: one row per session-graph node.
//!
//! Almost every statement over this table forks, and for one reason:
//! `tombstoned` is an integer on SQLite and a boolean on PostgreSQL, so
//! `tombstoned = 0` and `tombstoned = FALSE` are two texts, not one. ADR 0098
//! is explicit that a boolean literal is a fork rather than something to
//! template over, so the reachability predicate every read carries makes those
//! reads backend-only. What is left shared is the single-row insert, which
//! names no boolean.
//!
//! The fourteen SELECTs this table carried before this module collapse into
//! the seven named projections below: the same row, read through one of seven
//! documented column lists instead of through a column subset each call site
//! picked for itself.

/// The table's unprefixed name.
pub const TABLE: &str = "graph_nodes";

/// Every column, in insert order.
///
/// `tombstoned` is absent: it defaults to "live", and a node is only ever
/// tombstoned by [`GraphNodeStatements`]' backends' retire statement.
pub const INSERT_COLUMNS: &str =
    "session_id, node_id, parent_node_id, generation, frame_node_id, body_bytes, node_json";

crate::statements! {
    /// `graph_nodes` statements both backends issue verbatim.
    pub struct GraphNodeStatements @ "graph_node" {
        /// SQLite additionally declares a batch form over `json_each`; this
        /// single-row statement is what both backends issue per node, and what
        /// SQLite replays the batch through when a constraint violation has to
        /// be attributed to a row.
        insert = "INSERT INTO graph_nodes
             (session_id, node_id, parent_node_id, generation, frame_node_id, body_bytes, node_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
    }
}
