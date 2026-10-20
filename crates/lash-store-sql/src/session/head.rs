//! `session_head`: one durable head per session on both stores.

/// The table's unprefixed name.
pub const TABLE: &str = "session_head";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "session_id, head_revision";

/// The head leaf and the readable generation range around one candidate node,
/// read as one statement.
///
/// This projection spans four relations on purpose. A caller that is not the
/// node's owning session must prove the node is on the readable edge path, and
/// asking for the head leaf, the leaf's own row and the candidate range in
/// three statements would let the head move between them.
pub const READABLE_RANGE_COLUMNS: &str =
    "revision.leaf_node_id, head_node.generation, head_node.tombstoned,
                                    node.node_id, node.parent_node_id,
                                    node.generation, node.tombstoned";

crate::statements! {
    /// Durable session-head statements both backends issue verbatim.
    pub struct SessionHeadStatements @ "session_head" {

        /// The published head of `?1`.
        select_meta = "SELECT revision.config_ref, head.head_revision, revision.leaf_node_id, revision.checkpoint_ref,
                    leaf.frame_node_id
             FROM session_head AS head LEFT JOIN session_revisions AS revision
                 ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             LEFT JOIN graph_nodes AS leaf
                 ON leaf.node_id = revision.leaf_node_id
             WHERE head.session_id = ?1";


        /// One mandatory admission header and the selected window's raw head
        /// facts for a current/admitted read. Terminal selection has its own shape.
        /// Presence is separate from a NULL marker and a missing head, so
        /// generation refusal always precedes hydration and missing windows.
        select_window = "WITH selected AS (
             SELECT head.session_id, head.head_revision, revision.config_ref,
                    revision.leaf_node_id, revision.checkpoint_ref,
                    revision.head_revision AS retained_revision
             FROM session_head AS head
             LEFT JOIN session_revisions AS revision
               ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             WHERE head.session_id = ?1
         )
         SELECT selected.config_ref, selected.head_revision, selected.leaf_node_id,
                selected.checkpoint_ref, leaf.frame_node_id, selected.retained_revision,
                meta.session_id AS meta_session_id, meta.session_state_version,
                EXISTS(SELECT 1 FROM deleted_sessions WHERE session_id = ?1) AS deleted
         FROM (SELECT 1 AS singleton) AS request
         LEFT JOIN selected ON TRUE
         LEFT JOIN session_meta AS meta ON meta.session_id = ?1
         LEFT JOIN graph_nodes AS leaf ON leaf.node_id = selected.leaf_node_id";

        /// The terminal selector has its own indexed run predicate.
        select_terminal_window = "WITH selected AS (
             SELECT ended.session_id, ended.terminal_window_revision AS head_revision, revision.config_ref,
                    revision.leaf_node_id, revision.checkpoint_ref, revision.head_revision AS retained_revision
             FROM session_runs AS ended
             LEFT JOIN session_revisions AS revision
               ON revision.session_id = ended.session_id
              AND revision.head_revision = ended.terminal_window_revision
             WHERE ended.session_id = ?1 AND ended.run = ?2
               AND ended.terminal_kind IS NOT NULL AND ended.terminal_window_revision IS NOT NULL
         )
         SELECT selected.config_ref, selected.head_revision, selected.leaf_node_id,
                selected.checkpoint_ref, leaf.frame_node_id, selected.retained_revision,
                meta.session_id AS meta_session_id, meta.session_state_version,
                EXISTS(SELECT 1 FROM deleted_sessions WHERE session_id = ?1) AS deleted
         FROM (SELECT 1 AS singleton) AS request
         LEFT JOIN selected ON TRUE
         LEFT JOIN session_meta AS meta ON meta.session_id = ?1
         LEFT JOIN graph_nodes AS leaf ON leaf.node_id = selected.leaf_node_id";

        /// The published revision of `?1`, read inside the write transaction
        /// so the commit's head verdict decides over what is actually stored.
        select_revision = "SELECT head_revision FROM session_head WHERE session_id = ?1";


        /// What session deletion needs from `?1`'s head before removing it.
        select_reclaim = "SELECT revision.leaf_node_id, revision.checkpoint_ref
             FROM session_head AS head JOIN session_revisions AS revision
               ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             WHERE head.session_id = ?1";


        /// The config-only head a creating admission writes beside the
        /// catalog row (FIG-4099): revision 0, no leaf, no checkpoint.
        insert_created = "INSERT INTO session_head (session_id, head_revision)
                 VALUES (?1, 0)";


        delete_by_session = "DELETE FROM session_head WHERE session_id = ?1";


        /// Every live checkpoint root: every retained revision (including
        /// each head), and every retained admission base (FIG-3682).
        select_checkpoint_roots = "SELECT checkpoint_ref FROM session_revisions WHERE checkpoint_ref IS NOT NULL
                 UNION
                 SELECT admission_base_checkpoint_ref FROM session_meta
                 WHERE admission_base_checkpoint_ref IS NOT NULL";


        /// Replace `?1`'s stored head document with `?2`.
        set_head_json = "UPDATE blobs SET content = ?2
             WHERE hash = (SELECT revision.config_ref FROM session_head AS head
                 JOIN session_revisions AS revision USING (session_id, head_revision) WHERE head.session_id = ?1)";


        /// Replace `?1`'s stored head document with text no decoder accepts.
        corrupt_head_json = "UPDATE blobs SET content = ?2
             WHERE hash = (SELECT revision.config_ref FROM session_head AS head
                 JOIN session_revisions AS revision USING (session_id, head_revision) WHERE head.session_id = ?1)";
    }
}
