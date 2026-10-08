//! Session-core SQL with shared head statements and backend-specific locks and writes.

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::session::{
    fork_lineage::ForkLineageStatements, graph_nodes::GraphNodeStatements,
    head::SessionHeadStatements as SharedHeadStatements, meta::SessionMetaStatements,
    meta_pending_observer_intents::ObserverIntentStatements, pins::PinStatements,
    revisions::SessionRevisionStatements, turn_commits::TurnCommitStatements,
};

lash_store_sql::statements! {
    /// `session_meta` statements only PostgreSQL issues.
    pub(crate) struct SessionMetaPostgresStatements @ "session_meta" {
        insert = "INSERT INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms, owning_process_id,
              retention_kind, retention_last_turns)
             VALUES (?1, ?15, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, NULL, ?16,
                     ?17, ?18)
             ON CONFLICT (session_id) DO NOTHING";


        /// The stored relation of `?1`, share-locked for the duration of the
        /// metadata load's transaction.
        ///
        /// The lock is the fork. SQLite's read runs under the database's own
        /// single-writer lock and needs none; here the owner and
        /// observer-intent reads that follow must see the same row this one
        /// did.
        select_relation_for_share = "SELECT session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_node_id, source_session_id,
    source_node_id FROM session_meta WHERE session_id = ?1 FOR SHARE";

        /// The sole recorded session's relation, if this database holds
        /// exactly one.
        select_sole_relation_for_share = "SELECT session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_node_id, source_session_id,
    source_node_id FROM session_meta
             ORDER BY session_id ASC LIMIT 2 FOR SHARE";

        /// The durable session-state version marker of `?1`, row-locked so a
        /// concurrent admission cannot move it inside this transaction.
        select_state_version_for_update = "SELECT session_state_version FROM session_meta WHERE session_id = ?1 FOR UPDATE";

        exists_materialized = "SELECT EXISTS(
                 SELECT 1 FROM session_head WHERE session_id = ?1
                 UNION ALL
                 SELECT 1 FROM session_meta WHERE session_id = ?1
             )";

        /// The fork's unlocked fast path: two questions in one round trip, so
        /// an already-materialized target or a permanent tombstone is refused
        /// before any advisory lock is taken. Both are re-asked under the lock
        /// afterwards, which is what makes the fast path safe.
        exists_materialized_or_deleted = "SELECT
                EXISTS(
                    SELECT 1 FROM session_head WHERE session_id = ?1
                    UNION ALL
                    SELECT 1 FROM session_meta WHERE session_id = ?1
                ),
                EXISTS(
                    SELECT 1 FROM deleted_sessions WHERE session_id = ?1
                )";

        /// Every session this database knows, live, closing and deleted, with
        /// the observer-intent rows of each. `closing` is the live row's
        /// `closing_intent`, so a session whose close has begun never lists
        /// as live.
        select_catalog = "WITH catalog AS (
             SELECT meta.session_id, meta.relation_kind, meta.parent_session_id,
                    meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    meta.created_at_ms,
                    meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0) AS head_revision,
                    FALSE AS deleted,
                    meta.closing_intent IS NOT NULL AS closing
             FROM session_meta AS meta
             LEFT JOIN session_head AS session ON session.session_id = meta.session_id
             UNION ALL
             SELECT session_id, relation_kind,
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL,
                    created_at_ms, last_commit_at_ms,
                    head_revision, TRUE, FALSE
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
    /// Head statements requiring PostgreSQL semantics.
    pub(crate) struct SessionHeadPostgresStatements @ "session_head" {

        /// The published head of `?1`, row-locked.
        select_meta_for_update = "SELECT revision.head_json, head.head_revision, revision.leaf_node_id, revision.checkpoint_ref,
                leaf.frame_node_id
         FROM session_head AS head LEFT JOIN session_revisions AS revision
                 ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             LEFT JOIN graph_nodes AS leaf
             ON leaf.node_id = revision.leaf_node_id
         WHERE head.session_id = ?1 FOR UPDATE OF head";

        /// The published revision of `?1` under the commit's row lock: the
        /// authority the head verdict decides over.
        select_revision_for_update = "SELECT head_revision
             FROM session_head
             WHERE session_id = ?1
             FOR UPDATE";

        /// Publish the new revision over `?3`, after recording its row.
        /// The session advisory lock also serializes the first publication.
        upsert_cas = "INSERT INTO session_head
             (session_id, head_revision)
             VALUES (?1, ?2)
             ON CONFLICT (session_id) DO UPDATE SET
                head_revision = EXCLUDED.head_revision
             WHERE session_head.head_revision = ?3";

        insert_fork = "INSERT INTO session_head (session_id, head_revision) VALUES (?1, 0)";

        /// Every distinct checkpoint root the sessions in `?1` have published.
        select_checkpoints_for_sessions = "SELECT DISTINCT revision.checkpoint_ref
         FROM session_head AS head JOIN session_revisions AS revision
           ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
         WHERE head.session_id = ANY(?1) AND revision.checkpoint_ref IS NOT NULL
         ORDER BY checkpoint_ref";

        /// The first page of sessions that have published a checkpoint root,
        /// `?1` rows of it.
        ///
        /// `checkpoint_ref IS NOT NULL` is the definition of "has published a
        /// checkpoint root": a session without one has nothing durable at this
        /// level, and emitting a row for it would pad the report with items an
        /// operator cannot act on.
        ///
        /// Its resuming sibling is
        /// [`SessionHeadPostgresStatements::scan_checkpoints_after`]. Two statements, not
        /// one with `?1 IS NULL OR session_id > ?1`: that predicate is not
        /// sargable, so the paginated walk this exists to make cheap would
        /// scan the whole table on every page.
        scan_checkpoints_first_page = "SELECT head.session_id, revision.checkpoint_ref
     FROM session_head AS head JOIN session_revisions AS revision
       ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
     WHERE revision.checkpoint_ref IS NOT NULL
     ORDER BY head.session_id
     LIMIT ?1";

        /// The page of sessions that have published a checkpoint root after
        /// `?1`, `?2` rows of it.
        scan_checkpoints_after = "SELECT head.session_id, revision.checkpoint_ref
     FROM session_head AS head JOIN session_revisions AS revision
       ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
     WHERE revision.checkpoint_ref IS NOT NULL
       AND head.session_id > ?1
     ORDER BY head.session_id
     LIMIT ?2";

        /// Delete every head in `?1`, reporting the leaf nodes they published
        /// and whether any of them still has a live graph node.
        ///
        /// One statement because the two facts must come from the same delete:
        /// asking which leaves were removed and then asking whether anything
        /// live remains would let a concurrent commit land between them.
        delete_batch_returning = "WITH removed_sessions AS (
                 DELETE FROM session_head AS session
                 USING session_revisions AS revision
                 WHERE session.session_id = ANY(?1)
                   AND revision.session_id = session.session_id
                   AND revision.head_revision = session.head_revision
                 RETURNING session.session_id, revision.leaf_node_id
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
        select_head_json_for_update = "SELECT revision.head_json FROM session_head AS head
             JOIN session_revisions AS revision
               ON revision.session_id = head.session_id AND revision.head_revision = head.head_revision
             WHERE head.session_id = ?1 FOR UPDATE OF revision";
    }
}

lash_store_sql::statements! {
    /// `session_revisions` statements only PostgreSQL issues.
    pub(crate) struct SessionRevisionPostgresStatements @ "session_revision" {
        /// Revision `?2` of session `?1`, share-locked: a fork reads the
        /// retained point under a lock the collection's release waits on, so
        /// the fork's own head roots the checkpoint before the row can go.
        /// SQLite reads the same row with the shared statement, under its
        /// single-writer lock.
        select_for_share = "SELECT leaf_node_id, checkpoint_ref, head_json
             FROM session_revisions
             WHERE session_id = ?1 AND head_revision = ?2
             FOR SHARE";
    }
}

lash_store_sql::statements! {
    /// `graph_nodes` statements only PostgreSQL issues.
    pub(crate) struct GraphNodePostgresStatements @ "graph_node" {
        /// The live nodes of session `?1` among the ids in `?2`: each one's
        /// parent, body and stored size.
        select_live_owned_bodies = "SELECT node_id, parent_node_id, node_json, body_bytes
             FROM graph_nodes
             WHERE session_id = ?1 AND node_id = ANY(?2) AND tombstoned = FALSE";

        /// The same root classes as checkpoint reclamation. An admission
        /// conservatively protects its committed session nodes until released.
        artifact_frame_is_retained = "WITH RECURSIVE roots AS (
            SELECT leaf_node_id AS node_id FROM session_revisions WHERE leaf_node_id IS NOT NULL
            UNION SELECT node.node_id FROM graph_nodes AS node
                JOIN session_meta AS meta ON meta.session_id = node.session_id
                WHERE meta.admission_base_checkpoint_ref IS NOT NULL AND node.tombstoned = FALSE
        ), retained AS (
            SELECT node_id FROM roots
            UNION SELECT node.parent_node_id FROM graph_nodes AS node
                JOIN retained ON retained.node_id = node.node_id
                WHERE node.parent_node_id IS NOT NULL AND node.tombstoned = FALSE
        ) SELECT EXISTS(SELECT 1 FROM graph_nodes AS node JOIN retained USING(node_id)
            WHERE node.session_id = ?1 AND node.node_id = ?2 AND node.tombstoned = FALSE)";

        /// Take the row lock on live node `?1`, reporting whether it is there.
        lock_live = "SELECT TRUE FROM graph_nodes
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

        /// The generation, frame pointer and owner of the live leaf `?1`,
        /// row-locked for the duration of the commit.
        select_parent_facts_for_update = "SELECT generation, frame_node_id, session_id FROM graph_nodes
                 WHERE node_id = ?1 AND tombstoned = FALSE
                 FOR UPDATE";

        /// The frame node nearest to `?1`.
        select_frame_node_id = "SELECT frame_node_id FROM graph_nodes
         WHERE node_id = ?1 AND tombstoned = FALSE";

        /// The owner and generation of live node `?1` when session `?2`'s
        /// ownership-or-ceiling accelerator admits it at or below generation
        /// `?3`: the fresh-append ancestor fence's candidate. The head-path
        /// probe then confirms it through parent edges (ADR 0057).
        select_readable_ancestor = "SELECT node.session_id, node.generation FROM graph_nodes AS node
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
        /// at it, or a retained revision publishing it.
        exists_reachable = "SELECT
                EXISTS(
                    SELECT 1 FROM graph_nodes
                    WHERE parent_node_id = ?1 AND tombstoned = FALSE
                )
                OR EXISTS(
                    SELECT 1 FROM session_revisions WHERE leaf_node_id = ?1
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
               SELECT 1 FROM session_revisions AS revision
               WHERE revision.leaf_node_id = node.node_id
           )
         ORDER BY node.generation DESC";

        /// Every unreachable live leaf the session_head in `?1` still own, ordered
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
                   SELECT 1 FROM session_revisions AS revision
                   WHERE revision.leaf_node_id = node.node_id
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
        /// Record a receipt, staged on the turn feed: it takes the staging
        /// sequence's next value and no feed sequence, so its transaction
        /// takes no lock another writer waits on (FIG-5276).
        insert_staged = "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, outcome_code, committed_at_ms,
                request_identity_hash, requested_node_count, identity_encoding_version,
                failure_evidence, head_revision
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";

        /// Record session `?1`'s terminal, with fault `?2` at `?3`, staged on
        /// the turn feed as [`Self::insert_staged`] does.
        insert_session_terminal_staged = "INSERT INTO session_terminal_changes
             (session_id, fault_json, recorded_at_ms) VALUES (?1, ?2, ?3)";

        /// Whether a committed change of the turn feed waits for its sequence.
        has_unsequenced = "SELECT EXISTS (
                 SELECT 1 FROM runtime_turn_commits WHERE change_seq IS NULL
             ) OR EXISTS (
                 SELECT 1 FROM session_terminal_changes WHERE change_seq IS NULL
             )";

        /// The turn clock, under its write lock: what orders two sequencing
        /// transactions. Each later statement of the holder reads a snapshot
        /// that already holds every change the previous holder sequenced.
        lock_clock = "SELECT current_seq FROM turn_change_clock WHERE singleton = 1 FOR UPDATE";

        /// Give every committed change that has no feed sequence the next
        /// ones, in staging order, and move the clock past them. Run under
        /// [`Self::lock_clock`], after commit: a change is sequenced only
        /// once its transaction committed, and the sequencing transaction
        /// makes the whole batch visible at once, so a reader's cursor never
        /// passes a change that is sequenced later. A row a writer holds
        /// locked waits for the next run. Reports how many it sequenced.
        sequence_committed = "WITH receipts AS MATERIALIZED (
                 SELECT session_id, turn_id, staged_seq FROM runtime_turn_commits
                 WHERE change_seq IS NULL
                 FOR UPDATE SKIP LOCKED
             ), terminals AS MATERIALIZED (
                 SELECT staged_seq FROM session_terminal_changes
                 WHERE change_seq IS NULL
                 FOR UPDATE SKIP LOCKED
             ), staged AS MATERIALIZED (
                 SELECT staged_seq, row_number() OVER (ORDER BY staged_seq) AS ordinal
                 FROM (
                     SELECT staged_seq FROM receipts
                     UNION ALL
                     SELECT staged_seq FROM terminals
                 ) AS pending
             ), clock AS (
                 UPDATE turn_change_clock
                 SET current_seq = current_seq + (SELECT count(*) FROM staged)
                 WHERE singleton = 1 AND EXISTS (SELECT 1 FROM staged)
                 RETURNING current_seq - (SELECT count(*) FROM staged) AS base
             ), sequenced_receipts AS (
                 UPDATE runtime_turn_commits AS receipt
                 SET change_seq = clock.base + staged.ordinal
                 FROM receipts JOIN staged USING (staged_seq), clock
                 WHERE receipt.session_id = receipts.session_id
                   AND receipt.turn_id = receipts.turn_id
                 RETURNING 1
             ), sequenced_terminals AS (
                 UPDATE session_terminal_changes AS terminal
                 SET change_seq = clock.base + staged.ordinal
                 FROM terminals JOIN staged USING (staged_seq), clock
                 WHERE terminal.staged_seq = terminals.staged_seq
                 RETURNING 1
             )
             SELECT (SELECT count(*) FROM sequenced_receipts)
                  + (SELECT count(*) FROM sequenced_terminals)";

        /// Drop every receipt of a deleted session older than `?1`.
        delete_retained = "DELETE FROM runtime_turn_commits AS receipt
             WHERE receipt.committed_at_ms < ?1 AND receipt.change_seq <= ?2
               AND EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = receipt.session_id)";
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
             LEFT JOIN session_head AS session ON session.session_id = meta.session_id
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
         LEFT JOIN session_head AS session ON session.session_id = target.session_id
         WHERE EXISTS (
                   SELECT 1 FROM session_meta AS meta
                   WHERE meta.session_id = target.session_id
               )
            OR EXISTS (
                   SELECT 1 FROM session_head AS session
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
                       SELECT 1 FROM session_revisions AS revision
                       WHERE revision.checkpoint_ref = edge.checkpoint_ref
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM session_meta AS meta
                       WHERE meta.admission_base_checkpoint_ref = edge.checkpoint_ref
                   )";

        /// Sever dead-root edges touching the reclaim candidates in `?1`.
        ///
        /// Both outgoing and incoming edges go before any blob delete. The
        /// rooting predicates match `delete_unrooted`, including admissions.
        delete_unrooted_for_candidates = "DELETE FROM checkpoint_blob_refs AS edge
             WHERE (edge.checkpoint_ref = ANY(?1::TEXT[])
                    OR edge.blob_ref = ANY(?1::TEXT[]))
               AND NOT EXISTS (
                   SELECT 1 FROM session_revisions AS revision
                   WHERE revision.checkpoint_ref = edge.checkpoint_ref
               )
               AND NOT EXISTS (
                   SELECT 1 FROM session_meta AS meta
                   WHERE meta.admission_base_checkpoint_ref = edge.checkpoint_ref
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
    /// `fleet_format` statements. The installer seeds the row, and every
    /// open reads the recorded generation (ADR 0106 §1, ADR 0115 §2.1).
    pub(crate) struct FleetFormatStatements @ "fleet_format" {
        /// The recorded fleet format: the writer fence's read, once its
        /// transaction holds the fence lock (ADR 0115 §2.2).
        select_fleet_format = "SELECT format_version FROM fleet_format WHERE singleton = TRUE";

        /// A move of `F`, under the fence lock held exclusive.
        update_format_version = "UPDATE fleet_format SET format_version = ?1
             WHERE singleton = TRUE";

        /// Whether the row's relation exists at all: a migration fences only
        /// a catalog that can record `F`.
        select_is_present = "SELECT to_regclass('lash_fleet_format') IS NOT NULL";

        /// `lash migrate`'s seed: a recorded generation is never overwritten
        /// — the seed only provisions.
        insert_if_absent = "INSERT INTO fleet_format (
             singleton, format_version
         ) VALUES (TRUE, ?1)
         ON CONFLICT (singleton) DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `fleet_plugin_writers` statements: the fleet record's per-plugin
    /// writer ranges (FIG-4746). Every one runs under the writer fence's
    /// lock, held shared by a writer or exclusive by finalize.
    pub(crate) struct FleetPluginWriterStatements @ "fleet_plugin_writers" {
        /// Every recorded range.
        select_all = "SELECT plugin_id, min_format, max_format FROM fleet_plugin_writers";

        /// The ranges of the named plugins: what one publication is admitted
        /// against.
        select_named = "SELECT plugin_id, min_format, max_format FROM fleet_plugin_writers
             WHERE plugin_id = ANY(?1)";

        /// Provision a plugin the record does not name. A recorded range is
        /// left alone: two publications that both provision it agree on what
        /// the first one recorded.
        insert_if_absent = "INSERT INTO fleet_plugin_writers (plugin_id, min_format, max_format)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (plugin_id) DO NOTHING";

        /// Finalize's move of a range, under the lock that moves `F`.
        upsert = "INSERT INTO fleet_plugin_writers (plugin_id, min_format, max_format)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (plugin_id) DO UPDATE SET
                 min_format = EXCLUDED.min_format,
                 max_format = EXCLUDED.max_format";
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
    /// Shared session-head statements.
    pub(crate) head: SharedHeadStatements,
    pub(crate) head_postgres: SessionHeadPostgresStatements,
    /// `graph_nodes` statements both backends issue verbatim.
    pub(crate) graph: GraphNodeStatements,
    /// `graph_nodes` statements only PostgreSQL issues.
    pub(crate) graph_postgres: GraphNodePostgresStatements,
    /// `session_revisions` statements, the retained-revisions relation
    /// among them.
    pub(crate) revisions: SessionRevisionStatements,
    /// `session_revisions` statements only PostgreSQL issues.
    pub(crate) revisions_postgres: SessionRevisionPostgresStatements,
    /// `pins` statements.
    pub(crate) pins: PinStatements,
    /// `fork_lineage` statements.
    pub(crate) lineage: ForkLineageStatements,
    /// `runtime_turn_commits` statements both backends issue verbatim.
    pub(crate) turn_commits: TurnCommitStatements,
    /// `runtime_turn_commits` statements only PostgreSQL issues.
    pub(crate) turn_commits_postgres: TurnCommitPostgresStatements,
    /// `deleted_sessions` statements only PostgreSQL issues.
    pub(crate) deleted_postgres: DeletedSessionPostgresStatements,
    /// `checkpoint_blob_refs` statements only PostgreSQL issues.
    pub(crate) checkpoint_edges: CheckpointBlobRefPostgresStatements,
    /// `release_stamp` statements only PostgreSQL issues.
    pub(crate) release_stamp: ReleaseStampStatements,
    /// `fleet_format` statements only PostgreSQL issues.
    pub(crate) fleet_format: FleetFormatStatements,
    /// `fleet_plugin_writers` statements only PostgreSQL issues.
    pub(crate) fleet_plugin_writers: FleetPluginWriterStatements,
}

static SESSION_SQL: LazyLock<SessionSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    SessionSql {
        meta: SessionMetaStatements::render(dialect),
        meta_postgres: SessionMetaPostgresStatements::render(dialect),
        observer_intents: ObserverIntentStatements::render(dialect),
        head: SharedHeadStatements::render(dialect),
        head_postgres: SessionHeadPostgresStatements::render(dialect),
        graph: GraphNodeStatements::render(dialect),
        graph_postgres: GraphNodePostgresStatements::render(dialect),
        revisions: SessionRevisionStatements::render(dialect),
        revisions_postgres: SessionRevisionPostgresStatements::render(dialect),
        pins: PinStatements::render(dialect),
        lineage: ForkLineageStatements::render(dialect),
        turn_commits: TurnCommitStatements::render(dialect),
        turn_commits_postgres: TurnCommitPostgresStatements::render(dialect),
        deleted_postgres: DeletedSessionPostgresStatements::render(dialect),
        checkpoint_edges: CheckpointBlobRefPostgresStatements::render(dialect),
        release_stamp: ReleaseStampStatements::render(dialect),
        fleet_format: FleetFormatStatements::render(dialect),
        fleet_plugin_writers: FleetPluginWriterStatements::render(dialect),
    }
});

/// The session-core statements, rendered once at first use and never again.
pub(crate) fn session_sql() -> &'static SessionSql {
    &SESSION_SQL
}
