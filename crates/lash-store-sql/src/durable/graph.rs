//! The checkpoint graph: the neutral statements of the `graph` domain
//! (FIG-5887 §7, lane G, FIG-5890).
//!
//! The graph tables are the content-addressed node store's edges
//! (`blob_edges`), the graph-reference rows a commit owns roots through
//! (`effect_admissions`; the graph
//! columns of `exec_snapshots`, `run_records`, `turn_phases` and
//! `turn_namespaces` are their own domains' statements), and
//! the root catalog view (`graph_root_catalog`) that names every root of
//! every source. This file and the matching file in each dialect are the
//! only places their SQL may appear (`scripts/check-durable-sql.py`).
//!
//! The catalog is a view of the actual owning columns, never a maintained
//! cache: a peel or the orphan audit asks it whether anything still names a
//! node.

/// `blob_edges`: one row per child of an installed graph node, exactly its
/// envelope's child table, by node id, parent first. Opaque blobs have none.
pub const EDGES_TABLE: &str = "blob_edges";

/// `effect_admissions`: an execution's admitted effect, by effect identity,
/// to its Admission node.
pub const ADMISSIONS_TABLE: &str = "effect_admissions";

/// `graph_root_catalog`: the view of every root of every source.
pub const CATALOG_VIEW: &str = "graph_root_catalog";

crate::statements! {
    lazy
    /// Graph statements both backends issue verbatim.
    pub struct GraphStatements @ "durable_graph" {
        /// Admit effect `?2` of execution `?1`, member `?3`/`?4`/`?5`, to
        /// Admission node `?6` at epoch `?7`; nothing when it is admitted.
        /// A member with no explicit `?3` shares the execution's owner key.
        admission_put = "INSERT INTO effect_admissions
                 (exec_key, effect_id, owner_key, run_seq, member_ordinal, admission_ref,
                  written_epoch)
             SELECT i.exec, i.effect, CASE WHEN i.run IS NULL THEN NULL ELSE COALESCE(i.owner, i.exec) END,
                 i.run, i.member, i.admission, i.epoch
             FROM (SELECT ?1 AS exec, ?2 AS effect, ?3 AS owner, ?4 AS run,
                 ?5 AS member, COALESCE((SELECT id FROM blobs WHERE hash = ?6), CASE WHEN ?6 IS NOT NULL THEN -1 END) AS admission, ?7 AS epoch) AS i WHERE TRUE
             ON CONFLICT (exec_key, effect_id) DO NOTHING
             RETURNING exec_key";

        /// Remove effect `?2` of execution `?1`'s lookup to `?3`. No row when
        /// it is not `?3`.
        admission_remove = "DELETE FROM effect_admissions
             WHERE exec_key = ?1 AND effect_id = ?2
               AND admission_ref = COALESCE((SELECT id FROM blobs WHERE hash = ?3), CASE WHEN ?3 IS NOT NULL THEN -1 END)
             RETURNING exec_key";

        /// Effect `?2` of execution `?1`'s Admission node.
        admission_root = "SELECT (SELECT hash FROM blobs WHERE id = admission_ref) AS admission_ref FROM effect_admissions
             WHERE exec_key = ?1 AND effect_id = ?2";

        /// Drop every admission of execution `?1`: the cell its consumer
        /// retired. Returns each Admission node it held.
        admissions_clear = "DELETE FROM effect_admissions WHERE exec_key = ?1
             RETURNING (SELECT hash FROM blobs WHERE id = admission_ref) AS admission_ref";

        /// Drop every admission whose execution key starts with `?1`: the
        /// cells of one turn at its end, or of one session at its delete.
        /// Returns each Admission node it held.
        admissions_clear_under = "DELETE FROM effect_admissions
             WHERE substr(exec_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)
             RETURNING (SELECT hash FROM blobs WHERE id = admission_ref) AS admission_ref";

        /// Every admission of execution `?1`, by effect identity, with
        /// where its outcome was delivered and whether a checkpoint covers it.
        admissions = "SELECT effect_id, owner_key, run_seq, member_ordinal,
                    (SELECT hash FROM blobs WHERE id = admission_ref) AS admission_ref,
                    written_epoch, delivered_batch, delivered_position,
                    CASE WHEN covered THEN 1 ELSE 0 END AS covered
             FROM effect_admissions WHERE exec_key = ?1 ORDER BY effect_id";

        /// Record that effect `?2` of execution `?1` was delivered at
        /// position `?4` of batch `?3`. No row when it has no admission or
        /// its delivery is recorded.
        admission_deliver = "UPDATE effect_admissions
             SET delivered_batch = ?3, delivered_position = ?4
             WHERE exec_key = ?1 AND effect_id = ?2 AND delivered_batch IS NULL
             RETURNING exec_key";

        /// A checkpoint of execution `?1` covers every admission it holds:
        /// its journal starts again empty.
        admissions_cover = "UPDATE effect_admissions
             SET covered = TRUE, delivered_batch = NULL, delivered_position = NULL
             WHERE exec_key = ?1 AND NOT covered";

        /// Session `?1`'s revision `?2`'s checkpoint root.
        revision_root = "SELECT (SELECT hash FROM blobs WHERE id = checkpoint_ref) AS checkpoint_ref FROM session_revisions
             WHERE session_id = ?1 AND head_revision = ?2";

        /// Session `?1`'s admission base root.
        admission_base_root = "SELECT (SELECT hash FROM blobs WHERE id = admission_base_checkpoint_ref) AS admission_base_checkpoint_ref
             FROM session_meta
             WHERE session_id = ?1";

        /// Up to `?2` blobs after `?1`, in digest order, that no root names
        /// and no edge reaches: one page of the orphan audit.
        orphans_after = "SELECT candidate.hash FROM blobs AS candidate
             WHERE candidate.hash > ?1
               AND NOT EXISTS (
                   SELECT 1 FROM graph_root_catalog AS root WHERE root.root_ref = candidate.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM blob_edges AS edge WHERE edge.child_id = candidate.id
               )
             ORDER BY candidate.hash
             LIMIT ?2";

        /// How many roots the catalog names.
        catalog_count = "SELECT count(*) FROM graph_root_catalog";

        /// How many blobs are stored.
        blob_count = "SELECT count(*) FROM blobs";
    }
}
