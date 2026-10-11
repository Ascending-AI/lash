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
    "session_id, head_revision, leaf_node_id, checkpoint_ref, config_ref";

crate::statements! {
    /// `session_revisions` statements both backends issue verbatim.
    pub struct SessionRevisionStatements @ "session_revision" {
        /// Store config body `?2` once under its content hash `?1`.
        insert_config = "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, 0)
             ON CONFLICT (hash) DO NOTHING";

        /// Record revision `?2` of session `?1`: leaf `?3`, checkpoint root
        /// `?4` and the config reference `?5` it was published with.
        insert = "INSERT INTO session_revisions
                 (session_id, head_revision, leaf_node_id, checkpoint_ref, config_ref)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Revision `?2` of session `?1`, if it is retained.
        select = "SELECT revision.leaf_node_id, revision.checkpoint_ref, config.content
             FROM session_revisions AS revision
             LEFT JOIN blobs AS config ON config.hash = revision.config_ref
             WHERE revision.session_id = ?1 AND revision.head_revision = ?2";

        /// The published pointer of session `?1`: its head.
        select_head_revision = "SELECT head_revision FROM session_head
             WHERE session_id = ?1";

        /// Every retained revision of session `?1`, oldest first.
        select_by_session = "SELECT revision.head_revision, revision.leaf_node_id, revision.checkpoint_ref, config.content
             FROM session_revisions AS revision
             LEFT JOIN blobs AS config ON config.hash = revision.config_ref
             WHERE revision.session_id = ?1
             ORDER BY revision.head_revision";

        /// Every checkpoint root a retained revision publishes.
        select_checkpoint_roots = "SELECT DISTINCT checkpoint_ref FROM session_revisions
             WHERE checkpoint_ref IS NOT NULL";

        /// The distinct checkpoint roots session `?1`'s retained revisions
        /// publish, in hash order: what its deletion may reclaim.
        select_session_checkpoints = "SELECT DISTINCT checkpoint_ref FROM session_revisions
             WHERE session_id = ?1 AND checkpoint_ref IS NOT NULL
             UNION SELECT config_ref FROM session_revisions WHERE session_id = ?1
             ORDER BY checkpoint_ref";

        /// The distinct leaves session `?1`'s retained revisions publish:
        /// where its deletion starts ancestry retirement.
        select_session_leaves = "SELECT DISTINCT leaf_node_id FROM session_revisions
             WHERE session_id = ?1 AND leaf_node_id IS NOT NULL
             ORDER BY leaf_node_id";

        /// Every retained revision of session `?1`: its deletion.
        delete_by_session = "DELETE FROM session_revisions WHERE session_id = ?1";

        /// Number revision `?2` of session `?1` once, from its head counter.
        mark_terminal = "UPDATE session_revisions SET terminal_ordinal = (
                 SELECT terminal_ordinal + 1 FROM session_head WHERE session_id = ?1
             ) WHERE session_id = ?1 AND head_revision = ?2 AND terminal_ordinal IS NULL
             RETURNING terminal_ordinal";

        /// Publish session `?1`'s terminal counter `?2`.
        advance_terminal = "UPDATE session_head SET terminal_ordinal = ?2 WHERE session_id = ?1";

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
        prune_unretained = "WITH scoped_heads AS (
                 SELECT session_id, head_revision, terminal_ordinal FROM session_head
                 WHERE session_id = ?2
                 UNION ALL
                 SELECT session_id, head_revision, terminal_ordinal FROM session_head
                 WHERE ?2 IS NULL
             ), policies AS (
                 SELECT head.session_id, head.head_revision, head.terminal_ordinal,
                        COALESCE(meta.retention_kind, 'until_gc') AS kind,
                        COALESCE(meta.retention_last_turns, 0) AS last_turns
                 FROM scoped_heads AS head LEFT JOIN session_meta AS meta
                   ON meta.session_id = head.session_id
             ), candidates AS (
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN session_revisions AS revision ON revision.session_id = policies.session_id
                 WHERE policies.kind = 'head_only' OR (policies.kind = 'until_gc' AND ?1 = 1)
                 UNION ALL
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN session_revisions AS revision ON revision.session_id = policies.session_id
                   AND revision.terminal_ordinal IS NULL
                 WHERE policies.kind = 'last_turns'
                 UNION ALL
                 SELECT revision.session_id, revision.head_revision FROM policies
                 JOIN session_revisions AS revision ON revision.session_id = policies.session_id
                   AND revision.terminal_ordinal <= policies.terminal_ordinal - policies.last_turns
                 WHERE policies.kind = 'last_turns'
             ) DELETE FROM session_revisions AS revision
             WHERE (revision.session_id, revision.head_revision) IN (SELECT * FROM candidates)
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
             RETURNING session_id, leaf_node_id";
    }
}
