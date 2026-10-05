//! `session_revisions`: one row per published head revision a session still
//! retains.
//!
//! The name of a state is `(session_id, head_revision)`. Every head
//! publication inserts its row in the publishing transaction: a session's
//! creation, a fork and each head commit. The row records the leaf node and
//! the checkpoint root that revision published, and carries the head document
//! it was published with: the configuration a fork of the revision copies,
//! including any config command applied since its frame opened.
//!
//! **Membership is the retained-revisions relation.** A row exists exactly
//! while its revision is retained, and every reclaimer roots what it keeps in
//! this table: the checkpoint mark-and-sweep, the component-edge deletes,
//! session-delete blob reclaim, ancestry retirement and frame-artifact
//! retention. Rows leave it in two ways only: with their session, and through
//! [`SessionRevisionStatements::prune_unretained`], which is the single
//! statement that says which revisions a session keeps.

/// The table's unprefixed name.
pub const TABLE: &str = "session_revisions";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, head_revision, leaf_node_id, checkpoint_ref, head_json";

crate::statements! {
    /// `session_revisions` statements both backends issue verbatim.
    pub struct SessionRevisionStatements @ "session_revision" {
        /// Record revision `?2` of session `?1`: leaf `?3`, checkpoint root
        /// `?4` and the head document `?5` it was published with.
        insert = "INSERT INTO session_revisions
                 (session_id, head_revision, leaf_node_id, checkpoint_ref, head_json)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Revision `?2` of session `?1`, if it is retained.
        select = "SELECT leaf_node_id, checkpoint_ref, head_json
             FROM session_revisions
             WHERE session_id = ?1 AND head_revision = ?2";

        /// The published pointer of session `?1`: its head.
        select_head_revision = "SELECT head_revision FROM session_head
             WHERE session_id = ?1";

        /// Every retained revision of session `?1`, oldest first.
        select_by_session = "SELECT head_revision, leaf_node_id, checkpoint_ref, head_json
             FROM session_revisions
             WHERE session_id = ?1
             ORDER BY head_revision";

        /// Every checkpoint root a retained revision publishes.
        select_checkpoint_roots = "SELECT DISTINCT checkpoint_ref FROM session_revisions
             WHERE checkpoint_ref IS NOT NULL";

        /// The distinct checkpoint roots session `?1`'s retained revisions
        /// publish, in hash order: what its deletion may reclaim.
        select_session_checkpoints = "SELECT DISTINCT checkpoint_ref FROM session_revisions
             WHERE session_id = ?1 AND checkpoint_ref IS NOT NULL
             ORDER BY checkpoint_ref";

        /// The distinct leaves session `?1`'s retained revisions publish:
        /// where its deletion starts ancestry retirement.
        select_session_leaves = "SELECT DISTINCT leaf_node_id FROM session_revisions
             WHERE session_id = ?1 AND leaf_node_id IS NOT NULL
             ORDER BY leaf_node_id";

        /// Every retained revision of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_revisions WHERE session_id = ?1";

        /// The retained-revisions relation, stated once: release every
        /// revision of session `?2` (of every session when `?2` is NULL) that
        /// nothing retains, and report the leaf each released revision
        /// published.
        ///
        /// A revision is retained while any of these holds:
        ///
        /// * it is its session's published head;
        /// * a pin resolves to it: a revision pin names it, a turn pin names
        ///   a run whose terminal commit published it, or an input pin names
        ///   an input bound to such a run;
        /// * its session's retention policy covers it. `until_gc` covers
        ///   every revision until a host collection (`?1 = 1`) runs, and
        ///   nothing during one. `last_turns` covers the revisions the last
        ///   `retention_last_turns` committed terminal runs published.
        ///   `head_only` covers nothing. A session with no metadata row is
        ///   `until_gc`.
        ///
        /// A pin whose target has not resolved retains nothing yet, and a
        /// run that ended without a commit names no revision.
        prune_unretained = "DELETE FROM session_revisions AS revision
             WHERE revision.session_id = COALESCE(?2, revision.session_id)
               AND revision.head_revision != (
                   SELECT head.head_revision FROM session_head AS head
                   WHERE head.session_id = revision.session_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM pins AS pin
                   WHERE pin.session_id = revision.session_id
                     AND (
                         (pin.target_kind = 'revision'
                          AND pin.target_id = CAST(revision.head_revision AS TEXT))
                         OR (pin.target_kind = 'turn' AND EXISTS (
                             SELECT 1 FROM session_runs AS ended
                             WHERE ended.session_id = pin.session_id
                               AND ended.run = pin.target_id
                               AND ended.terminal_head_revision = revision.head_revision
                         ))
                         OR (pin.target_kind = 'input' AND EXISTS (
                             SELECT 1 FROM session_run_inputs AS bound
                             JOIN session_runs AS ended
                               ON ended.session_id = bound.session_id
                              AND ended.run = bound.run
                             WHERE bound.session_id = pin.session_id
                               AND bound.input_id = pin.target_id
                               AND ended.terminal_head_revision = revision.head_revision
                         ))
                     )
               )
               AND NOT (?1 = 0 AND NOT EXISTS (
                   SELECT 1 FROM session_meta AS meta
                   WHERE meta.session_id = revision.session_id
               ))
               AND NOT EXISTS (
                   SELECT 1 FROM session_meta AS meta
                   WHERE meta.session_id = revision.session_id
                     AND (
                         (meta.retention_kind = 'until_gc' AND ?1 = 0)
                         OR (meta.retention_kind = 'last_turns' AND EXISTS (
                             SELECT 1 FROM session_runs AS ended
                             WHERE ended.session_id = revision.session_id
                               AND ended.terminal_head_revision = revision.head_revision
                               AND (
                                   SELECT COUNT(*) FROM session_runs AS newer
                                   WHERE newer.session_id = ended.session_id
                                     AND newer.terminal_head_revision
                                         > ended.terminal_head_revision
                               ) < meta.retention_last_turns
                         ))
                     )
               )
             RETURNING session_id, leaf_node_id";
    }
}
