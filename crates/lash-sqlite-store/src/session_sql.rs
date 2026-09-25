//! The SQLite owner of the session-core family's statements.
//!
//! Every statement this store issues over `session_meta` and its two child
//! tables, `session_head`, `graph_nodes`, `node_anchors`, `fork_lineage`,
//! `runtime_turn_commits`, `deleted_sessions`, `usage_deltas`,
//! `release_stamp` and `checkpoint_blob_refs` is either one of
//! `lash_store_sql::session`'s shared statements or one of the dialect-only
//! statements declared here. No other module in this crate spells one.
//!
//! **Why so much of this family forks.** Three reasons, and none of them is
//! taste:
//!
//! * `graph_nodes.tombstoned`, `deleted_sessions`' catalog flag and the
//!   fork-point listing's `pinned` are integers here and booleans on
//!   PostgreSQL. ADR 0098 is explicit that a boolean literal is a fork, so
//!   every read carrying the reachability predicate is two statements.
//! * The durable head is spelled `session_head` here and `sessions` there, a
//!   name both backends have frozen. That makes *every* head statement
//!   dialect-only by construction.
//! * SQLite binds a variable-length list through `json_each` where PostgreSQL
//!   uses `unnest` or `= ANY(...)`, and writes through `INSERT OR IGNORE` /
//!   `INSERT OR REPLACE` where PostgreSQL writes `ON CONFLICT`.
//!
//! **One dialect, not three.** Unlike the effect family, nothing reaches these
//! tables through an `ATTACH`ed schema: the durable-core database is always
//! the connection's own `main`, whether the caller is a bound store, the
//! session-delete catalog connection, the fork catalog connection or the
//! retention sweep (which attaches the *journal* beside it, not the catalog).
//! So the set renders once, unqualified, exactly the way these statements have
//! always addressed their tables.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::session::{
    fork_lineage::ForkLineageStatements, graph_nodes::GraphNodeStatements,
    meta::SessionMetaStatements, meta_pending_observer_intents::ObserverIntentStatements,
    node_anchors::NodeAnchorStatements, turn_commits::TurnCommitStatements,
    usage_deltas::UsageDeltaStatements,
};

lash_store_sql::statements! {
    /// `session_meta` statements only SQLite issues.
    pub(crate) struct SessionMetaSqliteStatements @ "session_meta" {
        /// `INSERT OR IGNORE` is the fork: PostgreSQL spells the same
        /// decision `ON CONFLICT (session_id) DO NOTHING`. The row count is
        /// load-bearing either way — a zero means the session was already
        /// admitted and the caller must compare lineage instead.
        insert = "INSERT OR IGNORE INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms)
             VALUES (?1, ?19, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, NULL)";

        /// Forks with [`SessionMetaSqliteStatements::insert`], and again on
        /// the conflict alias: SQLite's is `excluded`, PostgreSQL's is
        /// `EXCLUDED`.
        upsert = "INSERT INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms)
             VALUES (?1, ?19, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
               relation_kind = excluded.relation_kind,
               parent_session_id = excluded.parent_session_id,
               caused_by_kind = excluded.caused_by_kind,
               caused_by_session_id = excluded.caused_by_session_id,
               caused_by_turn_id = excluded.caused_by_turn_id,
               caused_by_effect_id = excluded.caused_by_effect_id,
               caused_by_call_id = excluded.caused_by_call_id,
               caused_by_process_id = excluded.caused_by_process_id,
               caused_by_process_event_sequence = excluded.caused_by_process_event_sequence,
               caused_by_occurrence_id = excluded.caused_by_occurrence_id,
               caused_by_subscription_id = excluded.caused_by_subscription_id,
               caused_by_subscription_incarnation = excluded.caused_by_subscription_incarnation,
               caused_by_subscription_revision = excluded.caused_by_subscription_revision,
               caused_by_node_id = excluded.caused_by_node_id,
               source_session_id = excluded.source_session_id,
               source_node_id = excluded.source_node_id";

        /// The stored relation of `?1`.
        ///
        /// PostgreSQL reads the same projection under `FOR SHARE`, which
        /// SQLite has no equivalent of and no need for: the read runs inside a
        /// transaction on the single-writer database.
        select_relation = "SELECT session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id FROM session_meta WHERE session_id = ?1";

        /// The sole recorded session, if this catalog holds exactly one.
        ///
        /// Two rows are read so "exactly one" can be decided; the caller
        /// re-reads the row it picked through
        /// [`SessionMetaSqliteStatements::select_relation`]. PostgreSQL reads
        /// the whole relation here instead, because its read is already
        /// holding a share lock it would rather not take twice.
        select_sole_session_id = "SELECT session_id FROM session_meta ORDER BY session_id ASC LIMIT 2";

        /// Names the head table, so it forks on the name (ADR 0098);
        /// PostgreSQL additionally wraps it in `EXISTS(...)`.
        exists_materialized = "SELECT 1 FROM session_meta WHERE session_id = ?1
                     UNION ALL
                     SELECT 1 FROM session_head WHERE session_id = ?1
                     LIMIT 1";

        /// Every session this catalog knows, live and deleted, with the
        /// observer-intent rows of each.
        ///
        /// Forks three ways: the head table's name, the `deleted` flag's
        /// integer spelling, and `json_group_array` where PostgreSQL has
        /// `jsonb_agg`. It stays one statement because a listing assembled
        /// from four reads would report a session as live in one row and
        /// deleted in another.
        select_catalog = "WITH catalog AS (
             SELECT meta.session_id, meta.relation_kind, meta.parent_session_id,
                    meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_occurrence_id, meta.caused_by_subscription_id,
                    meta.caused_by_subscription_incarnation,
                    meta.caused_by_subscription_revision, meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    meta.created_at_ms,
                    meta.last_commit_at_ms, COALESCE(head.head_revision, 0), 0 AS deleted
             FROM session_meta AS meta
             LEFT JOIN session_head AS head ON head.session_id = meta.session_id
             UNION ALL
             SELECT session_id, COALESCE(relation_kind, 'root'),
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    created_at_ms, last_commit_at_ms, head_revision, 1
             FROM deleted_sessions
         )
         SELECT catalog.*,
                CASE WHEN deleted = 1 THEN '[]' ELSE (
                    SELECT json_group_array(
                        json_array(process_index, process_id, process_incarnation)
                    )
                    FROM (
                        SELECT process_index, process_id, process_incarnation
                        FROM session_meta_pending_observer_intents
                        WHERE session_id = catalog.session_id
                        ORDER BY process_index
                    )
                ) END
         FROM catalog
         ORDER BY created_at_ms ASC, session_id ASC";
    }
}

lash_store_sql::statements! {
    /// `session_head` statements. Every one of them is SQLite's alone: the
    /// table is spelled `sessions` on PostgreSQL (ADR 0098), so the name is
    /// the fork and there is nothing to share.
    pub(crate) struct SessionHeadStatements @ "session_head" {
        /// The published head of `?1`.
        select_meta = "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref,
                    pending_follow_on_json
             FROM session_head WHERE session_id = ?1";

        /// The follow-on `?1`'s head owes (ADR 0101 §3), read by every claim
        /// inside its write transaction.
        select_pending_follow_on = "SELECT pending_follow_on_json FROM session_head WHERE session_id = ?1";

        /// Raise `?1`'s pending follow-on to `?2`, only while the head still
        /// owes the follow-on `?3` (the recovery bound's fenced write). The
        /// head revision does not move.
        raise_pending_follow_on = "UPDATE session_head SET pending_follow_on_json = ?2
             WHERE session_id = ?1
               AND json_extract(pending_follow_on_json, '$.follow_on_turn_id') = ?3";

        /// The published revision of `?1`, read inside the write transaction
        /// so the commit's head verdict decides over what is actually stored.
        select_revision = "SELECT head_revision FROM session_head WHERE session_id = ?1";

        /// The leaf node `?1`'s head points at.
        select_leaf_node_id = "SELECT leaf_node_id FROM session_head WHERE session_id = ?1";

        /// What session deletion needs from `?1`'s head before removing it.
        select_reclaim = "SELECT leaf_node_id, checkpoint_ref FROM session_head WHERE session_id = ?1";

        /// The session that retains node `?1` through its own head.
        select_retained_by_leaf = "SELECT session_id, checkpoint_ref FROM session_head
                     WHERE leaf_node_id = ?1 AND checkpoint_ref IS NOT NULL
                     ORDER BY session_id LIMIT 1";

        /// Publish `?1`'s head.
        ///
        /// No revision predicate, and it needs none: the plan's revision was
        /// read inside this `BEGIN IMMEDIATE` transaction — SQLite's
        /// database-wide single-writer lock — and re-read under the same lock
        /// before the shared head verdict authorized this write.
        upsert = "INSERT OR REPLACE INTO session_head
                         (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref,
                          pending_follow_on_json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

        insert_fork = "INSERT INTO session_head
                 (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
                 VALUES (?1, ?2, 0, ?3, ?4)";

        delete_by_session = "DELETE FROM session_head WHERE session_id = ?1";

        /// Every live checkpoint root: heads that have published one, every
        /// explicit anchor, and every retained admission base (FIG-3682).
        select_checkpoint_roots = "SELECT checkpoint_ref FROM session_head WHERE checkpoint_ref IS NOT NULL
                 UNION
                 SELECT checkpoint_ref FROM node_anchors
                 UNION
                 SELECT admission_base_checkpoint_ref FROM session_meta
                 WHERE admission_base_checkpoint_ref IS NOT NULL";

        /// The retained checkpoint for node `?1`: an explicit anchor if there
        /// is one, otherwise the lowest-numbered session head that points at
        /// it.
        select_retained_checkpoint = "SELECT source_session_id, checkpoint_ref FROM (
                         SELECT source_session_id, checkpoint_ref, 0 AS priority
                         FROM node_anchors WHERE node_id = ?1
                         UNION ALL
                         SELECT session_id, checkpoint_ref, 1 AS priority
                         FROM session_head
                         WHERE leaf_node_id = ?1 AND checkpoint_ref IS NOT NULL
                     )
                     ORDER BY priority, source_session_id LIMIT 1";

        /// Every retained fork point, pinned ones first.
        select_fork_points = "SELECT node_id, checkpoint_ref, source_session_id, pinned
             FROM (
                 SELECT node_id, checkpoint_ref, source_session_id, pinned,
                        ROW_NUMBER() OVER (
                            PARTITION BY node_id ORDER BY priority, source_session_id
                        ) AS ordinal
                 FROM (
                     SELECT node_id, checkpoint_ref, source_session_id,
                            1 AS pinned, 0 AS priority
                     FROM node_anchors
                     UNION ALL
                     SELECT leaf_node_id, checkpoint_ref, session_id,
                            0 AS pinned, 1 AS priority
                     FROM session_head
                     WHERE leaf_node_id IS NOT NULL AND checkpoint_ref IS NOT NULL
                 )
             )
             WHERE ordinal = 1
             ORDER BY node_id";

        /// The head leaf of `?1` and the readable generation range from `?2`
        /// up to it.
        ///
        /// One statement on purpose: a membership proof assembled from a head
        /// read and a range read would let the head move between them.
        select_readable_range = "WITH readable_sessions AS (
                                 SELECT ?1 AS session_id, NULL AS generation_ceiling
                                 UNION ALL
                                 SELECT lineage.ancestor_session_id, lineage.fork_generation
                                 FROM fork_lineage AS lineage
                                 WHERE lineage.session_id = ?1
                             )
                             SELECT head.leaf_node_id, head_node.generation, head_node.tombstoned,
                                    node.node_id, node.parent_node_id,
                                    node.generation, node.tombstoned
                             FROM session_head AS head
                             LEFT JOIN graph_nodes AS head_node
                               ON head_node.node_id = head.leaf_node_id
                             LEFT JOIN readable_sessions AS readable ON TRUE
                             LEFT JOIN graph_nodes AS node
                               ON node.session_id = readable.session_id
                              AND node.generation BETWEEN ?2 AND head_node.generation
                              AND (
                                  readable.generation_ceiling IS NULL
                                  OR node.generation <= readable.generation_ceiling
                              )
                             WHERE head.session_id = ?1";

        /// The sole session this catalog holds, if it holds exactly one.
        ///
        /// A head row and a metadata row are each evidence of a session, and a
        /// session may have either without the other, so the union is the
        /// question. Two rows are read from each side so "exactly one" can be
        /// decided without counting the whole catalog.
        select_sole_bound_session_id = "SELECT session_id FROM (
                         SELECT session_id FROM (
                             SELECT session_id FROM session_head
                             LIMIT 2
                         )
                         UNION
                         SELECT session_id FROM (
                             SELECT session_id FROM session_meta
                             LIMIT 2
                         )
                     )
                     LIMIT 2";

        /// The stored head document of `?1`, for a test that wants to read or
        /// rewrite it behind the store's back.
        select_head_json = "SELECT head_json FROM session_head WHERE session_id = ?1";

        /// Replace `?1`'s stored head document with `?2`.
        set_head_json = "UPDATE session_head SET head_json = ?2 WHERE session_id = ?1";

        /// Replace `?1`'s stored head document with text no decoder accepts.
        corrupt_head_json = "UPDATE session_head SET head_json = '{not-current-json' WHERE session_id = ?1";
    }
}

lash_store_sql::statements! {
    /// `graph_nodes` statements only SQLite issues. Every one of them carries
    /// the reachability predicate, whose `tombstoned = 0` is an integer
    /// comparison where PostgreSQL's is a boolean one.
    pub(crate) struct GraphNodeSqliteStatements @ "graph_node" {
        /// The generation of `?1` and whether it is tombstoned.
        select_leaf_state = "SELECT generation, tombstoned FROM graph_nodes WHERE node_id = ?1";

        /// Every node session `?1` may read, oldest first.
        ///
        /// The whole-graph shape. Its generation-bounded sibling is
        /// [`GraphNodeSqliteStatements::select_readable_to_generation`]: one
        /// statement per filter shape, because a single statement carrying
        /// `?2 IS NULL OR generation <= ?2` cannot use an index for either.
        select_readable = "SELECT node.node_id, node.parent_node_id, node.node_json,
                node.generation, node.frame_node_id
                 FROM graph_nodes AS node
                 WHERE node.tombstoned = 0
                   AND (
                       node.session_id = ?1
                       OR EXISTS (
                           SELECT 1 FROM fork_lineage AS lineage
                           WHERE lineage.session_id = ?1
                             AND lineage.ancestor_session_id = node.session_id
                             AND node.generation <= lineage.fork_generation
                       )
                   )
                 ORDER BY node.generation ASC";

        /// Every node session `?1` may read up to generation `?2`, oldest
        /// first: the active-path shape.
        select_readable_to_generation = "SELECT node.node_id, node.parent_node_id, node.node_json,
                node.generation, node.frame_node_id
                 FROM graph_nodes AS node
                 WHERE node.tombstoned = 0
                   AND node.generation <= ?2
                   AND (
                       node.session_id = ?1
                       OR EXISTS (
                           SELECT 1 FROM fork_lineage AS lineage
                           WHERE lineage.session_id = ?1
                             AND lineage.ancestor_session_id = node.session_id
                             AND node.generation <= lineage.fork_generation
                       )
                   )
                 ORDER BY node.generation ASC";

        /// Node `?1`, if session `?2` may read it.
        select_lookup = "SELECT node.node_id, node.parent_node_id, node.node_json,
                node.session_id, node.generation
                         FROM graph_nodes AS node
                         WHERE node.node_id = ?1 AND node.tombstoned = 0
                           AND (
                               node.session_id = ?2
                               OR EXISTS (
                                   SELECT 1 FROM fork_lineage AS lineage
                                   WHERE lineage.session_id = ?2
                                     AND lineage.ancestor_session_id = node.session_id
                                     AND node.generation <= lineage.fork_generation
                               )
                           )";

        exists_live = "SELECT 1 FROM graph_nodes
                     WHERE node_id = ?1 AND tombstoned = 0";

        /// The owning session and generation of live node `?1`.
        select_owner_generation = "SELECT session_id, generation FROM graph_nodes
                     WHERE node_id = ?1 AND tombstoned = 0";

        /// One edge of the retained fork path at `?1`.
        select_edge = "SELECT node_id, parent_node_id, session_id, generation
                         FROM graph_nodes
                         WHERE node_id = ?1 AND tombstoned = 0";

        /// The body of frame node `?1`, for recovering a retained fork's
        /// configuration.
        select_frame_body = "SELECT parent_node_id, node_json FROM graph_nodes
             WHERE node_id = ?1 AND tombstoned = 0";

        /// The generation and frame pointer of the live leaf `?1`.
        select_parent_facts = "SELECT generation, frame_node_id FROM graph_nodes
                                 WHERE node_id = ?1 AND tombstoned = 0";

        /// The frame node nearest to `?1`.
        select_frame_node_id = "SELECT frame_node_id FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = 0";

        /// Whether session `?2` may read live node `?1` at or below
        /// generation `?3`: the fresh-append ancestor fence.
        exists_readable_ancestor = "SELECT 1 FROM graph_nodes AS node
                                 WHERE node.node_id = ?1
                                   AND node.tombstoned = 0
                                   AND node.generation <= ?3
                                   AND (
                                       node.session_id = ?2
                                       OR EXISTS (
                                           SELECT 1 FROM fork_lineage AS lineage
                                           WHERE lineage.session_id = ?2
                                             AND lineage.ancestor_session_id = node.session_id
                                             AND node.generation <= lineage.fork_generation
                                       )
                                   )";

        /// The parent of `?1` when `?1` itself is unreachable: no live child,
        /// no head pointing at it, no anchor holding it.
        select_retirable_parent = "SELECT node.parent_node_id
                 FROM graph_nodes AS node
                 WHERE node.node_id = ?1 AND node.tombstoned = 0
                   AND NOT EXISTS (
                       SELECT 1 FROM graph_nodes AS child
                       WHERE child.parent_node_id = node.node_id
                         AND child.tombstoned = 0
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM session_head AS head
                       WHERE head.leaf_node_id = node.node_id
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM node_anchors AS anchor
                       WHERE anchor.node_id = node.node_id
                   )";

        /// Every unreachable live leaf session `?1` still owns, newest first.
        select_unreachable_leaves = "SELECT node.node_id FROM graph_nodes AS node
                         WHERE node.session_id = ?1 AND node.tombstoned = 0
                           AND NOT EXISTS (
                               SELECT 1 FROM graph_nodes AS child
                               WHERE child.parent_node_id = node.node_id
                                 AND child.tombstoned = 0
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM session_head AS head
                               WHERE head.leaf_node_id = node.node_id
                           )
                           AND NOT EXISTS (
                               SELECT 1 FROM node_anchors AS anchor
                               WHERE anchor.node_id = node.node_id
                           )
                         ORDER BY node.generation DESC";

        retire = "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1";

        /// Which of the node ids in the JSON array `?1` already have a row.
        ///
        /// One statement per commit rather than one per node: the planner
        /// needs the whole occupied set before it decides anything. The ids
        /// ride as one JSON array, so the scalar-parameter ceiling is never in
        /// play. PostgreSQL binds a text array instead.
        select_occupied = "SELECT node_id FROM graph_nodes
                 WHERE node_id IN (SELECT value FROM json_each(?1))";

        /// One statement rather than one per node: the rows are known in full
        /// before any of them is written and they all land or none do, so a
        /// statement per node bought no atomicity and cost a round trip per
        /// node. A constraint violation is replayed through
        /// [`lash_store_sql::session::graph_nodes::GraphNodeStatements`]'
        /// single-row insert so the refusal names the offending row, which
        /// SQLite's batch error cannot.
        insert_batch = "INSERT INTO graph_nodes
             (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
             SELECT json_extract(node.value, '$[0]'),
                    json_extract(node.value, '$[1]'),
                    json_extract(node.value, '$[2]'),
                    json_extract(node.value, '$[3]'),
                    json_extract(node.value, '$[4]'),
                    json_extract(node.value, '$[5]')
             FROM json_each(?1) AS node";

        delete_tombstoned_for_session = "DELETE FROM graph_nodes
                     WHERE session_id = ?1 AND tombstoned = 1";

        /// Drop every tombstoned row owned by session `?1` or by a session
        /// that is already deleted.
        ///
        /// A node can be tombstoned after its owner is gone, and no
        /// session-scoped vacuum could ever reach it again: the owning id is
        /// permanently unbindable. Live sessions' rows stay resident for their
        /// own vacuum, so this is not a catalog-wide sweep.
        delete_tombstoned_reclaimable = "DELETE FROM graph_nodes
                 WHERE tombstoned = 1
                   AND (session_id = ?1
                        OR session_id IN (SELECT session_id FROM deleted_sessions))";
    }
}

lash_store_sql::statements! {
    /// `runtime_turn_commits` statements only SQLite issues.
    pub(crate) struct TurnCommitSqliteStatements @ "turn_commit" {
        /// The shape the sweep uses when no scope is still live. Its sibling
        /// [`TurnCommitSqliteStatements::delete_retained_except_live`] carries
        /// the exclusion list; they are two statements because an empty
        /// `NOT IN (...)` has no spelling and a `COALESCE`d one would scan.
        delete_retained = "DELETE FROM runtime_turn_commits AS receipt
                 WHERE receipt.committed_at_ms < ?1
                   AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = receipt.session_id)";

        /// Drop every receipt of a deleted session older than `?1` except the
        /// operation keys in the JSON array `?2`, which are the proof a later
        /// sweep needs that their scopes were still live.
        delete_retained_except_live = "DELETE FROM runtime_turn_commits AS receipt
                     WHERE receipt.committed_at_ms < ?1
                       AND receipt.turn_id NOT IN (SELECT value FROM json_each(?2))
                       AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                                   WHERE deleted.session_id = receipt.session_id)";
    }
}

lash_store_sql::statements! {
    /// `usage_deltas` statements only SQLite issues.
    pub(crate) struct UsageDeltaSqliteStatements @ "usage_delta" {
        /// PostgreSQL spells the same decision `ON CONFLICT ...
        /// DO NOTHING` over the identity columns.
        insert = "INSERT OR IGNORE INTO usage_deltas (
                                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json
                                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)";
    }
}

lash_store_sql::statements! {
    /// `deleted_sessions` statements only SQLite issues.
    pub(crate) struct DeletedSessionSqliteStatements @ "deleted_session" {
        /// Reads a row and lets the caller ask whether one came back;
        /// PostgreSQL asks `EXISTS(...)` and reads a boolean.
        exists = "SELECT 1 FROM deleted_sessions WHERE session_id = ?1";

        insert_from_meta = "INSERT OR IGNORE INTO deleted_sessions
                     (session_id, created_at_ms, last_commit_at_ms, head_revision,
                      relation_kind, parent_session_id)
                     SELECT meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                            COALESCE(head.head_revision, 0), meta.relation_kind,
                            meta.parent_session_id
                     FROM session_meta AS meta
                     LEFT JOIN session_head AS head ON head.session_id = meta.session_id
                     WHERE meta.session_id = ?1";

        /// Record `?1`'s permanent identity evidence when it has a head but no
        /// metadata row, so the deleted set still covers the id.
        insert_root = "INSERT OR IGNORE INTO deleted_sessions
                     (session_id, created_at_ms, last_commit_at_ms, head_revision,
                      relation_kind, parent_session_id)
                     VALUES (?1, 0, NULL, 0, 'root', NULL)";
    }
}

lash_store_sql::statements! {
    /// `checkpoint_blob_refs` statements only SQLite issues.
    pub(crate) struct CheckpointBlobRefSqliteStatements @ "checkpoint_blob_ref" {
        /// Record every edge from checkpoint `?1` to the component refs in the
        /// JSON array `?2`. PostgreSQL binds a text array through `unnest`.
        insert_batch = "INSERT OR IGNORE INTO checkpoint_blob_refs (checkpoint_ref, blob_ref)
                 SELECT ?1, CAST(value AS TEXT) FROM json_each(?2)";

        /// Every component of checkpoint `?1`, in hash order, which is the
        /// order the delete-time blob locks are taken in.
        select_components = "SELECT blob_ref FROM checkpoint_blob_refs
                         WHERE checkpoint_ref = ?1 ORDER BY blob_ref";

        /// Sever every edge whose checkpoint no longer has a live root.
        ///
        /// Runs before any hash-ordered blob delete: a component can sort
        /// before its root, and PostgreSQL's foreign key must never be
        /// weakened to accommodate stale ownership rows, so SQLite matches the
        /// ordering even though its side is not enforced.
        delete_unrooted = "DELETE FROM checkpoint_blob_refs AS edge
             WHERE NOT EXISTS (
                       SELECT 1 FROM session_head AS head
                       WHERE head.checkpoint_ref = edge.checkpoint_ref
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM node_anchors AS anchor
                       WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM session_meta AS meta
                       WHERE meta.admission_base_checkpoint_ref = edge.checkpoint_ref
                   )";

        /// Sever checkpoint `?1`'s outgoing edges when the owner transaction
        /// removed its final head or anchor.
        delete_unrooted_for_checkpoint = "DELETE FROM checkpoint_blob_refs AS edge
                     WHERE edge.checkpoint_ref = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM session_head AS head
                           WHERE head.checkpoint_ref = edge.checkpoint_ref
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM node_anchors AS anchor
                           WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                       )";
    }
}

lash_store_sql::statements! {
    /// `release_stamp` statements. All of them fork: SQLite's singleton flag
    /// is the integer `1` and PostgreSQL's is `TRUE`, and the write instant is
    /// the opening host's here and the server's there.
    pub(crate) struct ReleaseStampStatements @ "release_stamp" {
        /// The whole stamp.
        select_stamp = "SELECT release_version, schema_versions, written_at_epoch_ms
             FROM release_stamp WHERE singleton = 1";

        /// The writing release alone, for the update rule and for a refusal
        /// that has a release to name.
        select_release = "SELECT release_version FROM release_stamp WHERE singleton = 1";

        /// The caller has already applied the update rule, so this is reached
        /// only when the row must move.
        upsert = "INSERT INTO release_stamp (
             singleton, release_version, schema_versions, written_at_epoch_ms
         ) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(singleton) DO UPDATE SET
             release_version = excluded.release_version,
             schema_versions = excluded.schema_versions,
             written_at_epoch_ms = excluded.written_at_epoch_ms";
    }
}

/// Every session-core statement this store issues, rendered once.
pub(crate) struct SessionSql {
    /// `session_meta` statements both backends issue verbatim.
    pub(crate) meta: SessionMetaStatements,
    /// `session_meta` statements only SQLite issues.
    pub(crate) meta_sqlite: SessionMetaSqliteStatements,
    /// `session_meta_pending_observer_intents` statements.
    pub(crate) observer_intents: ObserverIntentStatements,
    /// `session_head` statements. SQLite's alone, by ADR 0098.
    pub(crate) head: SessionHeadStatements,
    /// `graph_nodes` statements both backends issue verbatim.
    pub(crate) graph: GraphNodeStatements,
    /// `graph_nodes` statements only SQLite issues.
    pub(crate) graph_sqlite: GraphNodeSqliteStatements,
    /// `node_anchors` statements.
    pub(crate) anchors: NodeAnchorStatements,
    /// `fork_lineage` statements.
    pub(crate) lineage: ForkLineageStatements,
    /// `runtime_turn_commits` statements both backends issue verbatim.
    pub(crate) turn_commits: TurnCommitStatements,
    /// `runtime_turn_commits` statements only SQLite issues.
    pub(crate) turn_commits_sqlite: TurnCommitSqliteStatements,
    /// `usage_deltas` statements both backends issue verbatim.
    pub(crate) usage: UsageDeltaStatements,
    /// `usage_deltas` statements only SQLite issues.
    pub(crate) usage_sqlite: UsageDeltaSqliteStatements,
    /// `deleted_sessions` statements only SQLite issues.
    pub(crate) deleted_sqlite: DeletedSessionSqliteStatements,
    /// `checkpoint_blob_refs` statements only SQLite issues.
    pub(crate) checkpoint_edges: CheckpointBlobRefSqliteStatements,
    /// `release_stamp` statements only SQLite issues.
    pub(crate) release_stamp: ReleaseStampStatements,
}

static SESSION_SQL: LazyLock<SessionSql> = LazyLock::new(|| {
    let dialect = Dialect::sqlite_unqualified();
    SessionSql {
        meta: SessionMetaStatements::render(dialect),
        meta_sqlite: SessionMetaSqliteStatements::render(dialect),
        observer_intents: ObserverIntentStatements::render(dialect),
        head: SessionHeadStatements::render(dialect),
        graph: GraphNodeStatements::render(dialect),
        graph_sqlite: GraphNodeSqliteStatements::render(dialect),
        anchors: NodeAnchorStatements::render(dialect),
        lineage: ForkLineageStatements::render(dialect),
        turn_commits: TurnCommitStatements::render(dialect),
        turn_commits_sqlite: TurnCommitSqliteStatements::render(dialect),
        usage: UsageDeltaStatements::render(dialect),
        usage_sqlite: UsageDeltaSqliteStatements::render(dialect),
        deleted_sqlite: DeletedSessionSqliteStatements::render(dialect),
        checkpoint_edges: CheckpointBlobRefSqliteStatements::render(dialect),
        release_stamp: ReleaseStampStatements::render(dialect),
    }
});

/// The session-core statements, rendered once at first use and never again.
pub(crate) fn session_sql() -> &'static SessionSql {
    &SESSION_SQL
}
