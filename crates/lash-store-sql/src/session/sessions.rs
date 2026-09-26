//! `sessions`: PostgreSQL's durable session head.
//!
//! The same logical table SQLite spells `session_head` ([`super::head`]). ADR
//! 0098 freezes both names, so every head statement is dialect-only by
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

/// The head as every loader decodes it: the full row minus the `session_id`
/// the read is keyed by.
pub const HEAD_META_COLUMNS: &str =
    "head_json, head_revision, leaf_node_id, checkpoint_ref, pending_follow_on_json";

/// A fork's head row, and a first commit's placeholder: every column but the
/// pending follow-on, which neither ever owes (ADR 0101 §3), so the column
/// stays NULL.
pub const FORK_INSERT_COLUMNS: &str =
    "session_id, head_revision, head_json, checkpoint_ref, leaf_node_id";

/// The pending follow-on alone: every claim reads it, and the recovery raise
/// rewrites it without touching the rest of the head (ADR 0101 §3).
pub const PENDING_FOLLOW_ON_COLUMNS: &str = "pending_follow_on_json";

/// What session deletion needs from the head before it removes the row.
pub const RECLAIM_COLUMNS: &str = "leaf_node_id, checkpoint_ref";

/// A session and the checkpoint root it has published.
///
/// The preflight walk pages over this projection; `head_json` is unbounded and
/// a drain report never decodes it.
pub const CHECKPOINT_SCAN_COLUMNS: &str = "session_id, checkpoint_ref";

/// The head half of the retained-checkpoint union, ranked behind an explicit
/// anchor. See [`super::node_anchors::RETAINED_PRIORITY_COLUMNS`].
pub const RETAINED_PRIORITY_COLUMNS: &str = "session_id, checkpoint_ref, 1 AS priority";

/// The head half of the fork-point listing: an unpinned retained point.
///
/// `pinned` is a real boolean here and an integer on SQLite, which is one of
/// the two reasons the listing forks at all.
pub const FORK_POINT_COLUMNS: &str =
    "leaf_node_id, checkpoint_ref, session_id, FALSE AS pinned, 1 AS priority";

/// The head leaf and the readable generation range around one candidate node,
/// read as one statement. See [`super::head::READABLE_RANGE_COLUMNS`].
pub const READABLE_RANGE_COLUMNS: &str = "session.leaf_node_id, head.generation, head.tombstoned,
                        node.node_id, node.parent_node_id,
                        node.generation, node.tombstoned";
