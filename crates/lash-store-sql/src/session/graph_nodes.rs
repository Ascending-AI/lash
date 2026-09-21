//! `graph_nodes`: one row per session-graph node.
//!
//! Almost every statement over this table forks, and for one reason:
//! `tombstoned` is an integer on SQLite and a boolean on PostgreSQL, so
//! `tombstoned = 0` and `tombstoned = FALSE` are two texts, not one. ADR 0098
//! is explicit that a boolean literal is a fork rather than something to
//! template over, so the reachability predicate every read carries makes those
//! reads dialect-only. What is left shared is the single-row insert, which
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
    "session_id, node_id, parent_node_id, generation, frame_node_id, node_json";

/// A readable graph row, as both backends fold a session's graph from it.
///
/// `session_id` and `tombstoned` are absent because the statement's predicate
/// has already decided them: every row this projection returns is live and
/// readable by the asking session.
pub const GRAPH_ROW_COLUMNS: &str = "node.node_id, node.parent_node_id, node.node_json,
                node.generation, node.frame_node_id";

/// One node looked up by id, with the owner the membership check compares.
///
/// `frame_node_id` is absent: a single-node read does not walk frames, and
/// `node_json` is the unbounded column this read must already pay for.
pub const NODE_LOOKUP_COLUMNS: &str = "node.node_id, node.parent_node_id, node.node_json,
                node.session_id, node.generation";

/// The leaf's generation and whether it is tombstoned, read together.
///
/// One statement rather than a live-only read plus a second "is it there at
/// all" probe: a missing leaf and a tombstoned leaf are the same refusal, and
/// asking twice would let the answer change between the two reads.
pub const LEAF_STATE_COLUMNS: &str = "generation, tombstoned";

/// What a commit needs from the node its new nodes descend from.
pub const PARENT_FACTS_COLUMNS: &str = "generation, frame_node_id";

/// The owning session and generation of a fork point.
pub const OWNER_GENERATION_COLUMNS: &str = "session_id, generation";

/// One edge of the retained fork path, as `ForkPlan::derive` consumes it.
pub const EDGE_PATH_COLUMNS: &str = "node_id, parent_node_id, session_id, generation";

/// The frame node's body, read to recover a retained fork's configuration.
pub const FRAME_BODY_COLUMNS: &str = "parent_node_id, node_json";

crate::statements! {
    /// `graph_nodes` statements both backends issue verbatim.
    pub struct GraphNodeStatements @ "graph_node" {
        /// Append one node of a commit's graph.
        ///
        /// SQLite additionally declares a batch form over `json_each`; this
        /// single-row statement is what both backends issue per node, and what
        /// SQLite replays the batch through when a constraint violation has to
        /// be attributed to a row.
        insert = "INSERT INTO graph_nodes
             (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    }
}
