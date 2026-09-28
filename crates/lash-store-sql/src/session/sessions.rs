//! `sessions`: PostgreSQL's durable session head.
//!
//! The same logical table SQLite spells `session_head` ([`super::head`]). ADR
//! 0098 freezes both names, so every head statement is backend-only by
//! construction and this module owns the PostgreSQL name and its column lists.

/// The table's unprefixed name.
pub const TABLE: &str = "sessions";

/// Every column, in insert order.
///
/// The order differs from SQLite's ([`super::head::INSERT_COLUMNS`]) because
/// both orders are what their backend's statements have always bound, and an
/// insert's column order is not something this arc changes.
pub const INSERT_COLUMNS: &str =
    "session_id, head_revision, head_json, checkpoint_ref, leaf_node_id, pending_follow_on_json";

/// The head half of the retained-checkpoint union, ranked behind an explicit
/// anchor. See [`super::node_anchors::RETAINED_PRIORITY_COLUMNS`].
pub const RETAINED_PRIORITY_COLUMNS: &str = "session_id, checkpoint_ref, 1 AS priority";

/// The head leaf and the readable generation range around one candidate node,
/// read as one statement. See [`super::head::READABLE_RANGE_COLUMNS`].
pub const READABLE_RANGE_COLUMNS: &str = "session.leaf_node_id, head.generation, head.tombstoned,
                        node.node_id, node.parent_node_id,
                        node.generation, node.tombstoned";
