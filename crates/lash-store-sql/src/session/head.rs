//! `session_head`: SQLite's durable session head — one row per session, its
//! published revision, its leaf node and its checkpoint root.
//!
//! PostgreSQL spells the same logical table `sessions` ([`super::sessions`]).
//! ADR 0098 freezes both names: renaming either would invalidate every
//! existing database for a cosmetic gain. The consequence is that no statement
//! over the head can ever be shared — the table name itself is the fork — so
//! this module owns the SQLite name and its column lists, and
//! `crates/lash-sqlite-store/src/session_sql.rs` owns the statements.

/// The table's unprefixed name.
pub const TABLE: &str = "session_head";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, head_json, head_revision, leaf_node_id, checkpoint_ref, pending_follow_on_json";

/// The head half of the retained-checkpoint union, ranked behind an explicit
/// anchor.
///
/// See [`super::node_anchors::RETAINED_PRIORITY_COLUMNS`]: the literal `1` is
/// the rank the enclosing `ORDER BY priority, source_session_id LIMIT 1`
/// consumes, which is what makes "an anchor outranks a head" a property of the
/// statement.
pub const RETAINED_PRIORITY_COLUMNS: &str = "session_id, checkpoint_ref, 1 AS priority";

/// The head leaf and the readable generation range around one candidate node,
/// read as one statement.
///
/// This projection spans three relations on purpose. A caller that is not the
/// node's owning session must prove the node is on the readable edge path, and
/// asking for the head leaf, the leaf's own row and the candidate range in
/// three statements would let the head move between them.
pub const READABLE_RANGE_COLUMNS: &str =
    "head.leaf_node_id, head_node.generation, head_node.tombstoned,
                                    node.node_id, node.parent_node_id,
                                    node.generation, node.tombstoned";
