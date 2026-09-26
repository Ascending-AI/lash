//! The PostgreSQL owner of the session-core family's statements.
//!
//! Every statement this store issues over `session_meta` and its two child
//! tables, `sessions`, `graph_nodes`, `node_anchors`, `fork_lineage`,
//! `runtime_turn_commits`, `deleted_sessions`, `usage_deltas`,
//! `release_stamp` and `checkpoint_blob_refs` is either one of
//! `lash_store_sql::session`'s shared statements or one of the dialect-only
//! statements declared here. No other module in this crate spells one.
//!
//! What forks and why is the mirror image of
//! `crates/lash-sqlite-store/src/session_sql.rs`: booleans where SQLite has
//! integers, `sessions` where SQLite has `session_head` (ADR 0098), `unnest` /
//! `= ANY(...)` where SQLite has `json_each`, `ON CONFLICT` where SQLite has
//! `INSERT OR IGNORE`, and — the one PostgreSQL has and SQLite does not —
//! `FOR UPDATE` and `FOR SHARE`, because `READ COMMITTED` cannot hold a read
//! across statements the way `BEGIN IMMEDIATE` can.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::session::{
    fork_lineage::ForkLineageStatements, graph_nodes::GraphNodeStatements,
    meta::SessionMetaStatements, meta_pending_observer_intents::ObserverIntentStatements,
    node_anchors::NodeAnchorStatements, turn_commits::TurnCommitStatements,
    usage_deltas::UsageDeltaStatements,
};

lash_store_sql::statements! {
    /// `session_meta` statements only PostgreSQL issues.
    pub(crate) struct SessionMetaPostgresStatements @ "session_meta" {
        insert = "INSERT INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms)
             VALUES (?1, ?19, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, NULL)
             ON CONFLICT (session_id) DO NOTHING";

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
             ON CONFLICT (session_id) DO UPDATE SET
               relation_kind = EXCLUDED.relation_kind,
               parent_session_id = EXCLUDED.parent_session_id,
               caused_by_kind = EXCLUDED.caused_by_kind,
               caused_by_session_id = EXCLUDED.caused_by_session_id,
               caused_by_turn_id = EXCLUDED.caused_by_turn_id,
               caused_by_effect_id = EXCLUDED.caused_by_effect_id,
               caused_by_call_id = EXCLUDED.caused_by_call_id,
               caused_by_process_id = EXCLUDED.caused_by_process_id,
               caused_by_process_event_sequence = EXCLUDED.caused_by_process_event_sequence,
               caused_by_occurrence_id = EXCLUDED.caused_by_occurrence_id,
               caused_by_subscription_id = EXCLUDED.caused_by_subscription_id,
               caused_by_subscription_incarnation = EXCLUDED.caused_by_subscription_incarnation,
               caused_by_subscription_revision = EXCLUDED.caused_by_subscription_revision,
               caused_by_node_id = EXCLUDED.caused_by_node_id,
               source_session_id = EXCLUDED.source_session_id,
               source_node_id = EXCLUDED.source_node_id";

        /// The stored relation of `?1`, share-locked for the duration of the
        /// metadata load's transaction.
        ///
        /// The lock is the fork. SQLite's read runs under the database's own
        /// single-writer lock and needs none; here the observer-intent and
        /// observer-intent reads that follow must see the same row this one
        /// did.
        select_relation_for_share = "SELECT session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id FROM session_meta WHERE session_id = ?1 FOR SHARE";

        /// The sole recorded session's relation, if this database holds
        /// exactly one.
        select_sole_relation_for_share = "SELECT session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id FROM session_meta
             ORDER BY session_id ASC LIMIT 2 FOR SHARE";

        /// The durable session-state version marker of `?1`, row-locked so a
        /// concurrent admission cannot move it inside this transaction.
        select_state_version_for_update = "SELECT session_state_version FROM session_meta WHERE session_id = ?1 FOR UPDATE";

        exists_materialized = "SELECT EXISTS(
                 SELECT 1 FROM sessions WHERE session_id = ?1
                 UNION ALL
                 SELECT 1 FROM session_meta WHERE session_id = ?1
             )";

        /// The fork's unlocked fast path: two questions in one round trip, so
        /// an already-materialized target or a permanent tombstone is refused
        /// before any advisory lock is taken. Both are re-asked under the lock
        /// afterwards, which is what makes the fast path safe.
        exists_materialized_or_deleted = "SELECT
                EXISTS(
                    SELECT 1 FROM sessions WHERE session_id = ?1
                    UNION ALL
                    SELECT 1 FROM session_meta WHERE session_id = ?1
                ),
                EXISTS(
                    SELECT 1 FROM deleted_sessions WHERE session_id = ?1
                )";

        /// Every session this database knows, live and deleted, with the
        /// observer-intent rows of each.
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
                    COALESCE(meta.created_at_ms, 0) AS created_at_ms,
                    meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0) AS head_revision,
                    FALSE AS deleted
             FROM session_meta AS meta
             LEFT JOIN sessions AS session ON session.session_id = meta.session_id
             UNION ALL
             SELECT session_id, COALESCE(relation_kind, 'root'),
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    COALESCE(created_at_ms, 0), last_commit_at_ms,
                    COALESCE(head_revision, 0), TRUE
             FROM deleted_sessions
         )
         SELECT catalog.*,
                CASE WHEN deleted THEN '[]' ELSE COALESCE((
                    SELECT jsonb_agg(
                               jsonb_build_array(process_index, process_id)
                               ORDER BY process_index
                           )::TEXT
                    FROM session_meta_pending_observer_intents
                    WHERE session_id = catalog.session_id
                ), '[]') END AS observer_intent_rows_json
         FROM catalog
         ORDER BY created_at_ms ASC, session_id ASC";
    }
}

lash_store_sql::statements! {
    /// `sessions` statements. Every one of them is PostgreSQL's alone: the
    /// table is spelled `session_head` on SQLite (ADR 0098), so the name is
    /// the fork and there is nothing to share.
    pub(crate) struct SessionsStatements @ "sessions" {
        /// The published head of `?1`.
        select_meta = "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref,
                pending_follow_on_json
         FROM sessions WHERE session_id = ?1";

        /// The published head of `?1`, row-locked.
        select_meta_for_update = "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref,
                pending_follow_on_json
         FROM sessions WHERE session_id = ?1 FOR UPDATE";

        /// The follow-on `?1`'s head owes (ADR 0101 §3), row-locked: every
        /// claim reads it inside its transaction, and the lock orders the read
        /// against a head commit or a recovery raise.
        select_pending_follow_on_for_share = "SELECT pending_follow_on_json FROM sessions
         WHERE session_id = ?1 FOR SHARE";

        /// The follow-on `?1`'s head owes, row-locked for the recovery raise
        /// and the commit that decides against it.
        select_pending_follow_on_for_update = "SELECT pending_follow_on_json FROM sessions
         WHERE session_id = ?1 FOR UPDATE";

        /// Raise `?1`'s pending follow-on to `?2`, only while the head still
        /// owes the follow-on `?3`. The head revision does not move.
        raise_pending_follow_on = "UPDATE sessions SET pending_follow_on_json = ?2
         WHERE session_id = ?1
           AND (pending_follow_on_json::jsonb ->> 'follow_on_turn_id') = ?3";

        /// The published revision of `?1`.
        select_revision = "SELECT head_revision FROM sessions WHERE session_id = ?1";

        /// The published revision of `?1` under the commit's row lock: the
        /// authority the head verdict decides over.
        select_revision_for_update = "SELECT head_revision
             FROM sessions
             WHERE session_id = ?1
             FOR UPDATE";

        /// Materialize a first commit's head row so the row lock below has a
        /// row to take.
        ///
        /// A head row does not exist during a session's first commit, so row
        /// locking alone cannot serialize create-versus-delete; this insert is
        /// what gives the lock something to hold.
        insert_placeholder = "INSERT INTO sessions
                 (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
                 VALUES (?1, 0, ?2, NULL, NULL)
                 ON CONFLICT (session_id) DO NOTHING";

        /// Publish `?1`'s head, if its stored revision is still `?6`.
        ///
        /// The revision predicate stays on the statement as the backstop, and
        /// it is the only statement-level guard for a concurrent *first*
        /// commit, whose placeholder row was created inside this transaction.
        /// For an existing session the row lock and the advisory lock have
        /// already settled the question and the shared verdict has authorized
        /// exactly this publication.
        upsert_cas = "INSERT INTO sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id,
              pending_follow_on_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?7)
             ON CONFLICT (session_id) DO UPDATE SET
                head_revision = EXCLUDED.head_revision,
                head_json = EXCLUDED.head_json,
                checkpoint_ref = EXCLUDED.checkpoint_ref,
                leaf_node_id = EXCLUDED.leaf_node_id,
                pending_follow_on_json = EXCLUDED.pending_follow_on_json
             WHERE sessions.head_revision = ?6";

        insert_fork = "INSERT INTO sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
             VALUES (?1, 0, ?2, ?3, ?4)";

        delete_by_session = "DELETE FROM sessions WHERE session_id = ?1";

        /// What session deletion needs from `?1`'s head before removing it.
        select_reclaim = "SELECT leaf_node_id, checkpoint_ref FROM sessions
         WHERE session_id = ?1";

        /// Every live checkpoint root: heads that have published one, every
        /// explicit anchor, and every retained admission base (FIG-3682).
        select_checkpoint_roots = "SELECT checkpoint_ref FROM sessions WHERE checkpoint_ref IS NOT NULL
             UNION
             SELECT checkpoint_ref FROM node_anchors
             UNION
             SELECT admission_base_checkpoint_ref FROM session_meta
             WHERE admission_base_checkpoint_ref IS NOT NULL";

        /// Every distinct checkpoint root the sessions in `?1` have published.
        select_checkpoints_for_sessions = "SELECT DISTINCT checkpoint_ref
         FROM sessions
         WHERE session_id = ANY(?1) AND checkpoint_ref IS NOT NULL
         ORDER BY checkpoint_ref";

        /// The retained checkpoint for node `?1`: an explicit anchor if there
        /// is one, otherwise the lowest-numbered session head that points at
        /// it.
        select_retained_checkpoint = "SELECT source_session_id, checkpoint_ref FROM (
             SELECT source_session_id, checkpoint_ref, 0 AS priority
             FROM node_anchors WHERE node_id = ?1
             UNION ALL
             SELECT session_id, checkpoint_ref, 1 AS priority FROM sessions
             WHERE leaf_node_id = ?1 AND checkpoint_ref IS NOT NULL
         ) retained
         ORDER BY priority, source_session_id LIMIT 1";

        /// Whether session `?2` still retains node `?1` at checkpoint `?3`,
        /// through an anchor or through its own head.
        exists_retention_source = "SELECT EXISTS(
             SELECT 1 FROM node_anchors
             WHERE node_id = ?1
               AND source_session_id = ?2
               AND checkpoint_ref = ?3
             UNION ALL
             SELECT 1 FROM sessions
             WHERE session_id = ?2
               AND leaf_node_id = ?1
               AND checkpoint_ref = ?3
         )";

        /// Every retained fork point, pinned ones first.
        select_fork_points = "SELECT node_id, checkpoint_ref, source_session_id, pinned
             FROM (
                 SELECT DISTINCT ON (node_id)
                        node_id, checkpoint_ref, source_session_id, pinned
                 FROM (
                     SELECT node_id, checkpoint_ref, source_session_id,
                            TRUE AS pinned, 0 AS priority
                     FROM node_anchors
                     UNION ALL
                     SELECT leaf_node_id, checkpoint_ref, session_id,
                            FALSE AS pinned, 1 AS priority
                     FROM sessions
                     WHERE leaf_node_id IS NOT NULL AND checkpoint_ref IS NOT NULL
                 ) candidates
                 ORDER BY node_id, priority, source_session_id
             ) retained
             ORDER BY node_id";

        /// The head leaf of `?1` and the readable generation range from `?2`
        /// up to it. See the SQLite twin for why it is one statement.
        select_readable_range = "WITH readable_sessions AS (
                     SELECT ?1::TEXT AS session_id, NULL::BIGINT AS generation_ceiling
                     UNION ALL
                     SELECT lineage.ancestor_session_id, lineage.fork_generation
                     FROM fork_lineage AS lineage
                     WHERE lineage.session_id = ?1
                 )
                 SELECT session.leaf_node_id, head.generation, head.tombstoned,
                        node.node_id, node.parent_node_id,
                        node.generation, node.tombstoned
                 FROM sessions AS session
                 LEFT JOIN graph_nodes AS head
                   ON head.node_id = session.leaf_node_id
                 LEFT JOIN readable_sessions AS readable ON TRUE
                 LEFT JOIN graph_nodes AS node
                   ON node.session_id = readable.session_id
                  AND node.generation BETWEEN ?2 AND head.generation
                  AND (
                      readable.generation_ceiling IS NULL
                      OR node.generation <= readable.generation_ceiling
                  )
                 WHERE session.session_id = ?1";

        /// The first page of sessions that have published a checkpoint root,
        /// `?1` rows of it.
        ///
        /// `checkpoint_ref IS NOT NULL` is the definition of "has published a
        /// checkpoint root": a session without one has nothing durable at this
        /// level, and emitting a row for it would pad the report with items an
        /// operator cannot act on.
        ///
        /// Its resuming sibling is
        /// [`SessionsStatements::scan_checkpoints_after`]. Two statements, not
        /// one with `?1 IS NULL OR session_id > ?1`: that predicate is not
        /// sargable, so the paginated walk this exists to make cheap would
        /// scan the whole table on every page.
        scan_checkpoints_first_page = "SELECT session_id, checkpoint_ref
     FROM sessions
     WHERE checkpoint_ref IS NOT NULL
     ORDER BY session_id
     LIMIT ?1";

        /// The page of sessions that have published a checkpoint root after
        /// `?1`, `?2` rows of it.
        scan_checkpoints_after = "SELECT session_id, checkpoint_ref
     FROM sessions
     WHERE checkpoint_ref IS NOT NULL
       AND session_id > ?1
     ORDER BY session_id
     LIMIT ?2";

        /// Delete every head in `?1`, reporting the leaf nodes they published
        /// and whether any of them still has a live graph node.
        ///
        /// One statement because the two facts must come from the same delete:
        /// asking which leaves were removed and then asking whether anything
        /// live remains would let a concurrent commit land between them.
        delete_batch_returning = "WITH removed_sessions AS (
                 DELETE FROM sessions AS session
                 WHERE session.session_id = ANY(?1)
                 RETURNING session.session_id, session.leaf_node_id
             )
             SELECT COALESCE(
                        array_agg(leaf_node_id ORDER BY session_id)
                            FILTER (WHERE leaf_node_id IS NOT NULL),
                        ARRAY[]::TEXT[]
                    ),
                    EXISTS (
                        SELECT 1
                        FROM graph_nodes AS graph
                        WHERE graph.tombstoned = FALSE
                          AND (
                              graph.session_id = ANY(?1)
                              OR graph.node_id IN (
                                  SELECT leaf_node_id FROM removed_sessions
                                  WHERE leaf_node_id IS NOT NULL
                              )
                          )
                    )
             FROM removed_sessions";

        /// The stored head document of `?1`, row-locked, for a test that wants
        /// to read or rewrite it behind the store's back.
        select_head_json_for_update = "SELECT head_json FROM sessions WHERE session_id = ?1 FOR UPDATE";

        /// Replace `?1`'s stored head document with `?2`.
        set_head_json = "UPDATE sessions SET head_json = ?2 WHERE session_id = ?1";

        /// Replace `?1`'s stored head document with text no decoder accepts.
        corrupt_head_json = "UPDATE sessions SET head_json = '{not-current-json' WHERE session_id = ?1";
    }
}

lash_store_sql::statements! {
    /// `graph_nodes` statements only PostgreSQL issues.
    pub(crate) struct GraphNodePostgresStatements @ "graph_node" {
        /// The generation of live node `?1`.
        ///
        /// SQLite reads the generation and the tombstone together, because it
        /// has to tell "missing" from "tombstoned" without a second read;
        /// here a missing row and a tombstoned row are the same `None` and the
        /// caller raises the same refusal.
        select_live_generation = "SELECT generation FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = FALSE";

        /// Every node session `?1` may read, oldest first: the whole-graph
        /// shape. See the SQLite twin for why the ceiling is a second
        /// statement rather than a nullable predicate.
        select_readable = "SELECT node.node_id, node.parent_node_id, node.node_json,
                node.generation, node.frame_node_id
         FROM graph_nodes AS node
         WHERE node.tombstoned = FALSE
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
         WHERE node.tombstoned = FALSE
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
             WHERE node.node_id = ?1 AND node.tombstoned = FALSE
               AND (
                   node.session_id = ?2
                   OR EXISTS (
                       SELECT 1 FROM fork_lineage AS lineage
                       WHERE lineage.session_id = ?2
                         AND lineage.ancestor_session_id = node.session_id
                         AND node.generation <= lineage.fork_generation
                   )
               )";

        /// Take the row lock on live node `?1`, reporting whether it is there.
        lock_live = "SELECT TRUE FROM graph_nodes
             WHERE node_id = ?1 AND tombstoned = FALSE
             FOR UPDATE";

        /// Take the row lock on live node `?1` without reading anything from
        /// it: the unpin path needs the lock ordered before the anchor delete,
        /// and nothing else.
        lock_live_id = "SELECT node_id FROM graph_nodes
             WHERE node_id = ?1 AND tombstoned = FALSE
             FOR UPDATE";

        /// The owning session and generation of live node `?1`, row-locked.
        select_owner_generation_for_update = "SELECT session_id, generation FROM graph_nodes
             WHERE node_id = ?1 AND tombstoned = FALSE
             FOR UPDATE";

        /// One edge of the retained fork path at `?1`, share-locked so the
        /// walk sees a stable path.
        select_edge_for_share = "SELECT node_id, parent_node_id, session_id, generation
                 FROM graph_nodes
                 WHERE node_id = ?1 AND tombstoned = FALSE
                 FOR SHARE";

        /// The body of frame node `?1`.
        select_frame_body = "SELECT parent_node_id, node_json FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = FALSE";

        /// The generation and frame pointer of the live leaf `?1`, row-locked
        /// for the duration of the commit.
        select_parent_facts_for_update = "SELECT generation, frame_node_id FROM graph_nodes
                 WHERE node_id = ?1 AND tombstoned = FALSE
                 FOR UPDATE";

        /// The frame node nearest to `?1`.
        select_frame_node_id = "SELECT frame_node_id FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = FALSE";

        /// Whether session `?2` may read live node `?1` at or below
        /// generation `?3`: the fresh-append ancestor fence.
        exists_readable_ancestor = "SELECT EXISTS(
                     SELECT 1 FROM graph_nodes AS node
                     WHERE node.node_id = ?1
                       AND node.tombstoned = FALSE
                       AND node.generation <= ?3
                       AND (
                           node.session_id = ?2
                           OR EXISTS (
                               SELECT 1 FROM fork_lineage AS lineage
                               WHERE lineage.session_id = ?2
                                 AND lineage.ancestor_session_id = node.session_id
                                 AND node.generation <= lineage.fork_generation
                           )
                       )
                 )";

        /// The parent of live node `?1`, row-locked.
        ///
        /// The ancestry walk is two statements here and one on SQLite: the
        /// lock must be taken before the reachability question is asked, and
        /// PostgreSQL cannot both lock a row and evaluate correlated
        /// anti-joins over other tables in the same statement.
        select_parent_for_update = "SELECT parent_node_id FROM graph_nodes
             WHERE node_id = ?1 AND tombstoned = FALSE
             FOR UPDATE";

        /// Whether node `?1` is still reachable: a live child, a head pointing
        /// at it, or an anchor holding it.
        exists_reachable = "SELECT
                EXISTS(
                    SELECT 1 FROM graph_nodes
                    WHERE parent_node_id = ?1 AND tombstoned = FALSE
                )
                OR EXISTS(
                    SELECT 1 FROM sessions WHERE leaf_node_id = ?1
                )
                OR EXISTS(
                    SELECT 1 FROM node_anchors WHERE node_id = ?1
                )";

        retire = "UPDATE graph_nodes SET tombstoned = TRUE WHERE node_id = ?1";

        /// Which of the node ids in `?1` already have a row.
        select_occupied = "SELECT node_id
             FROM graph_nodes
             WHERE node_id = ANY(?1)";

        /// Every unreachable live leaf session `?1` still owns, newest first.
        select_unreachable_leaves = "SELECT node.node_id FROM graph_nodes AS node
         WHERE node.session_id = ?1 AND node.tombstoned = FALSE
           AND NOT EXISTS (
               SELECT 1 FROM graph_nodes AS child
               WHERE child.parent_node_id = node.node_id
                 AND child.tombstoned = FALSE
           )
           AND NOT EXISTS (
               SELECT 1 FROM sessions AS head
               WHERE head.leaf_node_id = node.node_id
           )
           AND NOT EXISTS (
               SELECT 1 FROM node_anchors AS anchor
               WHERE anchor.node_id = node.node_id
           )
         ORDER BY node.generation DESC";

        /// Every unreachable live leaf the sessions in `?1` still own, ordered
        /// by owner and then newest first.
        ///
        /// The batch shape the process prune uses. Two statements rather than
        /// one over `= ANY(...)` with a single id: the single-session form is
        /// keyed on `session_id` and this one is not, so they take different
        /// plans and the batch's second `ORDER BY` key only makes sense here.
        select_unreachable_leaves_batch = "SELECT node.node_id FROM graph_nodes AS node
             WHERE node.session_id = ANY(?1) AND node.tombstoned = FALSE
               AND NOT EXISTS (
                   SELECT 1 FROM graph_nodes AS child
                   WHERE child.parent_node_id = node.node_id
                     AND child.tombstoned = FALSE
               )
               AND NOT EXISTS (
                   SELECT 1 FROM sessions AS head
                   WHERE head.leaf_node_id = node.node_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM node_anchors AS anchor
                   WHERE anchor.node_id = node.node_id
               )
             ORDER BY node.session_id, node.generation DESC";

        delete_tombstoned_for_session = "DELETE FROM graph_nodes WHERE session_id = ?1 AND tombstoned = TRUE";

        /// Drop every tombstoned row owned by session `?1` or by a session
        /// that is already deleted. See the SQLite twin for why the reclaim
        /// reaches past the named session.
        delete_tombstoned_reclaimable = "DELETE FROM graph_nodes
         WHERE tombstoned = TRUE
           AND (session_id = ?1
                OR session_id IN (SELECT session_id FROM deleted_sessions))";
    }
}

lash_store_sql::statements! {
    /// `runtime_turn_commits` statements only PostgreSQL issues.
    pub(crate) struct TurnCommitPostgresStatements @ "turn_commit" {
        /// The receipt session `?1` recorded for operation key `?2`, read in
        /// the round trip that settles turn `?3`'s park (FIG-3586) and logs
        /// the `Unparked{TurnCommitted}` event at `?4` (FIG-3659): a turn's
        /// commit clears its own park row inside the commit's transaction,
        /// and another turn's commit leaves it. A `NULL` `?3`, an operation
        /// that is no turn's, matches no park — and the `EXISTS` guard on the
        /// clock bump means no event, no sequence burned.
        ///
        /// A data-modifying `WITH` runs whether or not the outer query reads
        /// it, so the clear and the feed append cost the commit no round trip
        /// of their own.
        select_receipt_settling_turn_park = "WITH settled_park AS (
                 DELETE FROM turn_parks WHERE session_id = ?1 AND turn_id = ?3
                 RETURNING session_id, turn_id, park_id
             ), settled_park_clock AS (
                 UPDATE turn_park_clock SET current_seq = current_seq + 1
                 WHERE singleton = TRUE
                   AND EXISTS (SELECT 1 FROM settled_park)
                 RETURNING current_seq
             ), settled_park_event AS (
                 INSERT INTO turn_park_events
                     (seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms)
                 SELECT clock.current_seq, park.session_id, park.turn_id, park.park_id,
                        'unparked', 'turn_committed', NULL,
                        floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
                 FROM settled_park AS park
                 CROSS JOIN settled_park_clock AS clock
             )
             SELECT turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM runtime_turn_commits
                 WHERE session_id = ?1 AND turn_id = ?2";

        /// Drop every receipt of a deleted session older than `?1`.
        delete_retained = "DELETE FROM runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < ?1
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = receipt.session_id)";
    }
}

lash_store_sql::statements! {
    /// `usage_deltas` statements only PostgreSQL issues.
    pub(crate) struct UsageDeltaPostgresStatements @ "usage_delta" {
        insert = "INSERT INTO usage_deltas (
                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)
                 DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `deleted_sessions` statements only PostgreSQL issues.
    pub(crate) struct DeletedSessionPostgresStatements @ "deleted_session" {
        exists = "SELECT EXISTS(
                SELECT 1 FROM deleted_sessions WHERE session_id = ?1
             )";

        insert_from_meta = "INSERT INTO deleted_sessions
             (session_id, created_at_ms, last_commit_at_ms, head_revision,
              relation_kind, parent_session_id)
             SELECT meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0), meta.relation_kind,
                    meta.parent_session_id
             FROM session_meta AS meta
             LEFT JOIN sessions AS session ON session.session_id = meta.session_id
             WHERE meta.session_id = ?1
             ON CONFLICT (session_id) DO NOTHING";

        insert_root = "INSERT INTO deleted_sessions
             (session_id, created_at_ms, last_commit_at_ms, head_revision,
              relation_kind, parent_session_id)
             VALUES (?1, 0, NULL, 0, 'root', NULL)
             ON CONFLICT (session_id) DO NOTHING";

        /// The batch shape the process prune uses. A process-owned id may have
        /// a head and no metadata row, so the evidence is `COALESCE`d against
        /// the target list rather than against the metadata row, and the
        /// `WHERE EXISTS` pair is what keeps an id that was never materialized
        /// out of the deleted set.
        insert_batch_from_targets = "INSERT INTO deleted_sessions
         (session_id, created_at_ms, last_commit_at_ms, head_revision,
          relation_kind, parent_session_id)
         SELECT target.session_id, COALESCE(meta.created_at_ms, 0),
                meta.last_commit_at_ms, COALESCE(session.head_revision, 0),
                COALESCE(meta.relation_kind, 'root'), meta.parent_session_id
         FROM unnest(?1::TEXT[]) AS target(session_id)
         LEFT JOIN session_meta AS meta ON meta.session_id = target.session_id
         LEFT JOIN sessions AS session ON session.session_id = target.session_id
         WHERE EXISTS (
                   SELECT 1 FROM session_meta AS meta
                   WHERE meta.session_id = target.session_id
               )
            OR EXISTS (
                   SELECT 1 FROM sessions AS session
                   WHERE session.session_id = target.session_id
               )
         ON CONFLICT (session_id) DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `checkpoint_blob_refs` statements only PostgreSQL issues.
    pub(crate) struct CheckpointBlobRefPostgresStatements @ "checkpoint_blob_ref" {
        /// Record every edge from checkpoint `?1` to the component refs in the
        /// text array `?2`.
        insert_batch = "INSERT INTO checkpoint_blob_refs (checkpoint_ref, blob_ref)
         SELECT ?1, component_ref
           FROM unnest(?2::text[]) AS component_ref
         ON CONFLICT (checkpoint_ref, blob_ref) DO NOTHING";

        /// Every component of the checkpoints in `?1`, in hash order, which is
        /// the order the delete-time blob locks are taken in.
        select_components = "SELECT DISTINCT blob_ref
                 FROM checkpoint_blob_refs
                 WHERE checkpoint_ref = ANY(?1::TEXT[])
                 ORDER BY blob_ref";

        /// Sever every edge whose checkpoint no longer has a live root.
        delete_unrooted = "DELETE FROM checkpoint_blob_refs AS edge
             WHERE NOT EXISTS (
                       SELECT 1 FROM sessions AS head
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

        /// Sever the outgoing edges of every checkpoint in `?1` that stopped
        /// being a live root in this transaction.
        ///
        /// A root may remain as another root's opaque component, so deleting
        /// roots one at a time could not give this ordering; shared live roots
        /// keep both their row and their projection edges.
        delete_unrooted_for_checkpoints = "DELETE FROM checkpoint_blob_refs AS edge
             WHERE edge.checkpoint_ref = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM sessions AS head
                   WHERE head.checkpoint_ref = edge.checkpoint_ref
               )
               AND NOT EXISTS (
                   SELECT 1 FROM node_anchors AS anchor
                   WHERE anchor.checkpoint_ref = edge.checkpoint_ref
               )";
    }
}

lash_store_sql::statements! {
    /// `release_stamp` statements. All of them fork; see the SQLite twin.
    pub(crate) struct ReleaseStampStatements @ "release_stamp" {
        /// A host-provisioned deployment can admit a role holding nothing but
        /// `SELECT`, and that is a published property of that mode rather than
        /// an accident. The privilege is asked for with a catalog read instead
        /// of discovered by letting an `INSERT` raise `42501`: a refused
        /// statement would poison the admitting transaction, so the open would
        /// fail rather than proceed unstamped.
        ///
        /// The one place in this crate where the `lash_` prefix is spelled
        /// rather than rendered: `to_regclass` and `has_table_privilege` take
        /// the relation as *text*, which the renderer's token rewriter cannot
        /// reach.
        select_is_writable = "SELECT CASE
                  WHEN to_regclass('lash_release_stamp') IS NULL THEN FALSE
                  ELSE has_table_privilege('lash_release_stamp', 'INSERT')
                       AND has_table_privilege('lash_release_stamp', 'UPDATE')
                END";

        /// The whole stamp.
        select_stamp = "SELECT release_version, schema_versions, written_at_epoch_ms
         FROM release_stamp WHERE singleton = TRUE";

        /// The writing release alone.
        select_release = "SELECT release_version FROM release_stamp WHERE singleton = TRUE";

        /// `written_at_epoch_ms` is the PostgreSQL server's clock, not the
        /// opening host's: two hosts opening one database would otherwise
        /// stamp it from two unrelated clocks.
        upsert = "INSERT INTO release_stamp (
             singleton, release_version, schema_versions, written_at_epoch_ms
         ) VALUES (
             TRUE, ?1, ?2, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
         )
         ON CONFLICT (singleton) DO UPDATE SET
             release_version = EXCLUDED.release_version,
             schema_versions = EXCLUDED.schema_versions,
             written_at_epoch_ms = EXCLUDED.written_at_epoch_ms";
    }
}

lash_store_sql::statements! {
    /// `fleet_format` statements. A shared store *provisions* the row rather
    /// than finalizing it on open the way the SQLite twin does: the first
    /// opener inserts this build's fleet format, every later open reads the
    /// recorded generation, and only `finalize-upgrade` (FIG-3800) ever moves
    /// it (ADR 0106 §1).
    pub(crate) struct FleetFormatStatements @ "fleet_format" {
        /// Asked for, never discovered by a refused write inside the
        /// admitting transaction — the reason the release-stamp twin exists.
        /// The `lash_` prefix is spelled for the same reason too: the
        /// catalog functions take the relation as *text*, which the
        /// renderer's token rewriter cannot reach.
        select_is_writable = "SELECT CASE
                  WHEN to_regclass('lash_fleet_format') IS NULL THEN FALSE
                  ELSE has_table_privilege('lash_fleet_format', 'INSERT')
                END";

        /// The recorded fleet format.
        select_fleet_format = "SELECT format_version FROM fleet_format WHERE singleton = TRUE";

        /// Provision only: a later open never overwrites the fleet's recorded
        /// generation — moving the row is the finalize operation's job alone.
        insert_if_absent = "INSERT INTO fleet_format (
             singleton, format_version
         ) VALUES (TRUE, ?1)
         ON CONFLICT (singleton) DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// The one statement of this family that is not about a single table: the
    /// process-prune cascade, which removes a batch of sessions' rows from
    /// every table a session owns, in one statement.
    pub(crate) struct SessionCoreStatements @ "session_core" {
        /// One statement rather than fourteen, and that is the point: the parts
        /// would race. The prune has already taken each session's advisory
        /// lock, but a delete split across fourteen statements leaves thirteen
        /// windows in which a peer can observe a session whose queue is gone
        /// and whose metadata is not, and the tombstoned-row reclaim in the
        /// first arm depends on the ancestry retire that ran before it.
        ///
        /// The reclaim arm reaches past the batch on purpose: the ancestry
        /// retire tombstones a node regardless of who owns it, so a batch can
        /// strand a row belonging to a session outside it. That owner is
        /// unbindable, so no session-scoped vacuum could ever reach the row.
        /// Live sessions' rows stay resident for their own vacuum.
        delete_process_session_rows = "WITH deleted_graph_nodes AS (
             DELETE FROM graph_nodes
             WHERE tombstoned = TRUE
               AND (session_id = ANY(?1)
                    OR session_id IN (SELECT session_id FROM deleted_sessions))
             RETURNING node_id
         ),
         deleted_queued_work_items AS (
             DELETE FROM queued_work_items AS item
             WHERE EXISTS (
                 SELECT 1 FROM queued_work_batches AS batch
                 WHERE batch.batch_id = item.batch_id
                   AND batch.session_id = ANY(?1)
             )
             RETURNING item.batch_id
         ),
         deleted_queued_run_members AS (
             DELETE FROM queued_run_members
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_queued_runs AS (
             DELETE FROM queued_runs
             WHERE session_id = ANY(?1)
               AND (SELECT count(*) FROM deleted_queued_run_members) >= 0
             RETURNING session_id
         ),
         deleted_queued_work_batches AS (
             DELETE FROM queued_work_batches
             WHERE session_id = ANY(?1)
               AND (SELECT count(*) FROM deleted_queued_work_items) >= 0
             RETURNING batch_id
         ),
         deleted_wake_redelivery_fences AS (
             DELETE FROM wake_redelivery_fences
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_wake_allocation_floors AS (
             DELETE FROM wake_allocation_floors
             WHERE target_session_id = ANY(?1)
             RETURNING target_session_id
         ),
         deleted_pending_turn_inputs AS (
             DELETE FROM pending_turn_inputs
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_turn_parks AS (
             DELETE FROM turn_parks
             WHERE session_id = ANY(?1)
             RETURNING session_id, turn_id, park_id
         ),
         -- Every deleted park gets a Cancelled{SessionDeleted} event at the
         -- transaction's own instant: the ledger is the only place the park
         -- transition stays durable once the session rows are gone
         -- (FIG-3659). The batch bump allocates the block's sequences in one
         -- update — and only when a park was actually deleted, so a batch
         -- with no parks neither locks nor bumps the clock nor costs a
         -- clock-probe round trip; each event takes
         -- first_seq + row_number - 1.
         deleted_turn_park_clock AS (
             UPDATE turn_park_clock
             SET current_seq = current_seq + (SELECT count(*) FROM deleted_turn_parks)
             WHERE singleton = TRUE
               AND EXISTS (SELECT 1 FROM deleted_turn_parks)
             RETURNING current_seq - (SELECT count(*) FROM deleted_turn_parks) + 1 AS first_seq
         ),
         deleted_turn_park_events AS (
             INSERT INTO turn_park_events
                 (seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms)
             SELECT clock.first_seq + row_number() OVER (ORDER BY park.session_id) - 1,
                    park.session_id, park.turn_id, park.park_id,
                    'cancelled', 'session_deleted', NULL,
                    floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             FROM deleted_turn_parks AS park
             CROSS JOIN deleted_turn_park_clock AS clock
         ),
         deleted_session_ingress AS (
             DELETE FROM session_ingress
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_turn_cancel_requests AS (
             DELETE FROM turn_cancel_requests
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_turn_cancel_closures AS (
             DELETE FROM turn_cancel_closure_authorizations
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_turn_cancellation_bindings AS (
             DELETE FROM turn_cancellation_bindings
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_session_execution_leases AS (
             DELETE FROM session_execution_leases
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_fork_lineage AS (
             DELETE FROM fork_lineage
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_session_meta AS (
             DELETE FROM session_meta
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_session_roots AS (
             DELETE FROM session_roots
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         deleted_session_root_inputs AS (
             DELETE FROM session_root_inputs
             WHERE session_id = ANY(?1)
             RETURNING session_id
         ),
         -- A `close_session` intent outlives its session: it is the
         -- deletion tombstone the session's roots answer from.
         deleted_control_intents AS (
             DELETE FROM control_intents
             WHERE session_id = ANY(?1) AND kind <> 'close_session'
             RETURNING session_id
         )
         SELECT (SELECT count(*) FROM deleted_graph_nodes)
              + (SELECT count(*) FROM deleted_queued_run_members)
              + (SELECT count(*) FROM deleted_queued_runs)
              + (SELECT count(*) FROM deleted_queued_work_batches)
              + (SELECT count(*) FROM deleted_wake_redelivery_fences)
              + (SELECT count(*) FROM deleted_wake_allocation_floors)
              + (SELECT count(*) FROM deleted_pending_turn_inputs)
              + (SELECT count(*) FROM deleted_turn_parks)
              + (SELECT count(*) FROM deleted_session_ingress)
              + (SELECT count(*) FROM deleted_turn_cancel_closures)
              + (SELECT count(*) FROM deleted_turn_cancellation_bindings)
              + (SELECT count(*) FROM deleted_session_execution_leases)
              + (SELECT count(*) FROM deleted_fork_lineage)
              + (SELECT count(*) FROM deleted_session_meta)
              + (SELECT count(*) FROM deleted_session_roots)
              + (SELECT count(*) FROM deleted_session_root_inputs)
              + (SELECT count(*) FROM deleted_control_intents)";
    }
}

/// Every session-core statement this store issues, rendered once.
pub(crate) struct SessionSql {
    /// `session_meta` statements both backends issue verbatim.
    pub(crate) meta: SessionMetaStatements,
    /// `session_meta` statements only PostgreSQL issues.
    pub(crate) meta_postgres: SessionMetaPostgresStatements,
    /// `session_meta_pending_observer_intents` statements.
    pub(crate) observer_intents: ObserverIntentStatements,
    /// `sessions` statements. PostgreSQL's alone, by ADR 0098.
    pub(crate) head: SessionsStatements,
    /// `graph_nodes` statements both backends issue verbatim.
    pub(crate) graph: GraphNodeStatements,
    /// `graph_nodes` statements only PostgreSQL issues.
    pub(crate) graph_postgres: GraphNodePostgresStatements,
    /// `node_anchors` statements.
    pub(crate) anchors: NodeAnchorStatements,
    /// `fork_lineage` statements.
    pub(crate) lineage: ForkLineageStatements,
    /// `runtime_turn_commits` statements both backends issue verbatim.
    pub(crate) turn_commits: TurnCommitStatements,
    /// `runtime_turn_commits` statements only PostgreSQL issues.
    pub(crate) turn_commits_postgres: TurnCommitPostgresStatements,
    /// `usage_deltas` statements both backends issue verbatim.
    pub(crate) usage: UsageDeltaStatements,
    /// `usage_deltas` statements only PostgreSQL issues.
    pub(crate) usage_postgres: UsageDeltaPostgresStatements,
    /// `deleted_sessions` statements only PostgreSQL issues.
    pub(crate) deleted_postgres: DeletedSessionPostgresStatements,
    /// `checkpoint_blob_refs` statements only PostgreSQL issues.
    pub(crate) checkpoint_edges: CheckpointBlobRefPostgresStatements,
    /// `release_stamp` statements only PostgreSQL issues.
    pub(crate) release_stamp: ReleaseStampStatements,
    /// `fleet_format` statements only PostgreSQL issues.
    pub(crate) fleet_format: FleetFormatStatements,
    /// The cross-table process-prune cascade.
    pub(crate) core: SessionCoreStatements,
}

static SESSION_SQL: LazyLock<SessionSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    SessionSql {
        meta: SessionMetaStatements::render(dialect),
        meta_postgres: SessionMetaPostgresStatements::render(dialect),
        observer_intents: ObserverIntentStatements::render(dialect),
        head: SessionsStatements::render(dialect),
        graph: GraphNodeStatements::render(dialect),
        graph_postgres: GraphNodePostgresStatements::render(dialect),
        anchors: NodeAnchorStatements::render(dialect),
        lineage: ForkLineageStatements::render(dialect),
        turn_commits: TurnCommitStatements::render(dialect),
        turn_commits_postgres: TurnCommitPostgresStatements::render(dialect),
        usage: UsageDeltaStatements::render(dialect),
        usage_postgres: UsageDeltaPostgresStatements::render(dialect),
        deleted_postgres: DeletedSessionPostgresStatements::render(dialect),
        checkpoint_edges: CheckpointBlobRefPostgresStatements::render(dialect),
        release_stamp: ReleaseStampStatements::render(dialect),
        fleet_format: FleetFormatStatements::render(dialect),
        core: SessionCoreStatements::render(dialect),
    }
});

/// The session-core statements, rendered once at first use and never again.
pub(crate) fn session_sql() -> &'static SessionSql {
    &SESSION_SQL
}
