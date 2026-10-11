//! The checkpoint graph: the neutral statements of the `graph` domain
//! (FIG-5887 §7, lane G, FIG-5890).
//!
//! The graph tables are the content-addressed node store's edges
//! (`blob_edges`), the graph-reference rows a commit owns roots through
//! (`effect_admissions`, `graph_release_pins`; the graph
//! columns of `exec_snapshots`, `run_records`, `turn_phases` and
//! `turn_namespaces` are their own domains' statements), and
//! the root catalog view (`graph_root_catalog`) that names every root of
//! every source. This file and the matching file in each dialect are the
//! only places their SQL may appear (`scripts/check-durable-sql.py`).
//!
//! The catalog is a view of the actual owning columns, never a maintained
//! cache: a collector enumerates it one source at a time and marks from what
//! it reads.

/// `blob_edges`: one row per child of an installed graph node, exactly its
/// envelope's child table. Opaque blobs have none.
pub const EDGES_TABLE: &str = "blob_edges";

/// `effect_admissions`: an execution's admitted effect, by effect identity,
/// to its Admission node.
pub const ADMISSIONS_TABLE: &str = "effect_admissions";

/// `graph_release_pins`: an installed runnable release manifest.
pub const RELEASE_PINS_TABLE: &str = "graph_release_pins";

/// `graph_root_catalog`: the view of every root of every source.
pub const CATALOG_VIEW: &str = "graph_root_catalog";

crate::statements! {
    /// Graph statements both backends issue verbatim.
    pub struct GraphStatements @ "durable_graph" {
        /// Install blob `?1` with bytes `?2` under encoding `?3`; nothing
        /// when the address is taken (the caller compares).
        insert_blob = "INSERT INTO blobs (hash, content, encoding) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO NOTHING";

        /// Blob `?1`'s bytes and encoding.
        blob = "SELECT content, encoding FROM blobs WHERE hash = ?1";

        /// Blob `?1`'s encoding, when it exists.
        blob_encoding = "SELECT encoding FROM blobs WHERE hash = ?1";

        /// Record the edge from node `?1` to child `?2`.
        insert_edge = "INSERT INTO blob_edges (parent_hash, child_hash) VALUES (?1, ?2)
             ON CONFLICT (parent_hash, child_hash) DO NOTHING";

        /// Node `?1`'s stored edges, in digest order.
        edges_of = "SELECT child_hash FROM blob_edges WHERE parent_hash = ?1 ORDER BY child_hash";

        /// Admit effect `?2` of execution `?1`, member `?3`/`?4`/`?5`, to
        /// Admission node `?6` at epoch `?7`; nothing when it is admitted.
        admission_put = "INSERT INTO effect_admissions
                 (exec_key, effect_id, owner_key, run_seq, member_ordinal, admission_ref,
                  written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (exec_key, effect_id) DO NOTHING
             RETURNING exec_key";

        /// Remove effect `?2` of execution `?1`'s lookup to `?3`. No row when
        /// it is not `?3`.
        admission_remove = "DELETE FROM effect_admissions
             WHERE exec_key = ?1 AND effect_id = ?2 AND admission_ref = ?3
             RETURNING exec_key";

        /// Effect `?2` of execution `?1`'s Admission node.
        admission_root = "SELECT admission_ref FROM effect_admissions
             WHERE exec_key = ?1 AND effect_id = ?2";

        /// Drop every admission of execution `?1`: the cell its consumer
        /// retired.
        admissions_clear = "DELETE FROM effect_admissions WHERE exec_key = ?1";

        /// Drop every admission whose execution key starts with `?1`: the
        /// cells of one turn at its end, or of one session at its delete.
        admissions_clear_under = "DELETE FROM effect_admissions
             WHERE substr(exec_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)";

        /// The Admission node of every admission whose execution key starts
        /// with `?1`.
        admission_roots_under = "SELECT admission_ref FROM effect_admissions
             WHERE substr(exec_key, 1, length(CAST(?1 AS TEXT))) = CAST(?1 AS TEXT)";

        /// Every admission of execution `?1`, by effect identity.
        admissions = "SELECT effect_id, owner_key, run_seq, member_ordinal, admission_ref,
                    written_epoch
             FROM effect_admissions WHERE exec_key = ?1 ORDER BY effect_id";

        /// Session `?1`'s revision `?2`'s checkpoint root.
        revision_root = "SELECT checkpoint_ref FROM session_revisions
             WHERE session_id = ?1 AND head_revision = ?2";

        /// Session `?1`'s admission base root.
        admission_base_root = "SELECT admission_base_checkpoint_ref FROM session_meta
             WHERE session_id = ?1";

        /// Release `?1`'s pinned manifest.
        release_root = "SELECT root_ref FROM graph_release_pins WHERE release_key = ?1";

        /// Pin release `?1`'s manifest `?2`; nothing when it is pinned.
        release_install = "INSERT INTO graph_release_pins (release_key, root_ref) VALUES (?1, ?2)
             ON CONFLICT (release_key) DO NOTHING";

        /// Retire release `?1`'s pin on `?2`. No row when it is not `?2`.
        release_retire = "DELETE FROM graph_release_pins WHERE release_key = ?1 AND root_ref = ?2
             RETURNING release_key";

        /// Every root of source `?1`, as the catalog names it.
        catalog_source = "SELECT root_ref FROM graph_root_catalog WHERE source = ?1";
    }
}
