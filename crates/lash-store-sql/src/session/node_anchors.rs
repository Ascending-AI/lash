//! `node_anchors`: one row per explicitly pinned fork point.
//!
//! An anchor is the durable statement that a node's checkpoint must survive
//! even when no session head points at it any more, so the GC roots and the
//! delete-time reclaim both read this table beside the head.

/// The table's unprefixed name.
pub const TABLE: &str = "node_anchors";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "node_id, checkpoint_ref, source_session_id";

/// What a pin lookup reads back: the anchor minus the node id it was keyed by.
pub const ANCHOR_COLUMNS: &str = "checkpoint_ref, source_session_id";

/// The anchor half of the retained-checkpoint union, which ranks an explicit
/// anchor ahead of a session head that happens to point at the same node.
///
/// The literal `0` is the rank, not a column: the union's other arm supplies
/// `1`, and the enclosing `ORDER BY priority, source_session_id LIMIT 1` is
/// what makes "an anchor wins" a property of the statement rather than of the
/// caller.
pub const RETAINED_PRIORITY_COLUMNS: &str = "source_session_id, checkpoint_ref, 0 AS priority";

/// The anchor half of the fork-point listing, which reports every retained
/// point and whether it is pinned.
pub const FORK_POINT_COLUMNS_SQLITE: &str =
    "node_id, checkpoint_ref, source_session_id, 1 AS pinned, 0 AS priority";

/// The PostgreSQL spelling of [`FORK_POINT_COLUMNS_SQLITE`]: the same
/// projection with a boolean literal, which PostgreSQL's `pinned` column is
/// and SQLite's integer one is not.
pub const FORK_POINT_COLUMNS_POSTGRES: &str =
    "node_id, checkpoint_ref, source_session_id, TRUE AS pinned, 0 AS priority";

crate::statements! {
    /// `node_anchors` statements both backends issue verbatim.
    pub struct NodeAnchorStatements @ "node_anchor" {
        /// The anchor pinned at `?1`, if the node is pinned at all.
        select_by_node = "SELECT checkpoint_ref, source_session_id
             FROM node_anchors WHERE node_id = ?1";

        /// Pin `?1` at checkpoint `?2`, retained by session `?3`.
        insert = "INSERT INTO node_anchors (node_id, checkpoint_ref, source_session_id)
             VALUES (?1, ?2, ?3)";

        /// Unpin `?1`.
        delete_by_node = "DELETE FROM node_anchors WHERE node_id = ?1";
    }
}
