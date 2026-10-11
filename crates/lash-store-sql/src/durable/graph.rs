//! The checkpoint graph: the neutral statements of the `graph` domain
//! (FIG-5887 §7, lane G, FIG-5890).
//!
//! The graph tables are the content-addressed node store's edges
//! (`blob_edges`), the graph-reference rows a commit owns roots through
//! (`effect_admissions`, `blob_read_pins`, `graph_release_pins`, and the
//! graph columns of `exec_snapshots`, `run_records`, `turn_phases` and
//! `turn_namespaces`), the commit receipts (`graph_commit_receipts`), and
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

/// `blob_read_pins`: a streaming reader's durable pin on one root.
pub const READ_PINS_TABLE: &str = "blob_read_pins";

/// `graph_release_pins`: an installed runnable release manifest.
pub const RELEASE_PINS_TABLE: &str = "graph_release_pins";

/// `graph_commit_receipts`: a commit's retry identity, batch digest and
/// result, for the owner's retry lifetime. It owns no root.
pub const RECEIPTS_TABLE: &str = "graph_commit_receipts";

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

        /// Execution `?1`'s revision and graph root.
        exec_root = "SELECT rev, root_ref FROM exec_snapshots WHERE exec_key = ?1";

        /// Execution `?1`'s graph root row.
        exec_row = "SELECT rev, root_ref, written_epoch, commit_id FROM exec_snapshots
             WHERE exec_key = ?1 AND root_ref IS NOT NULL";

        /// Insert execution `?1`'s first root `?2` by commit `?3` at epoch `?4`.
        exec_insert = "INSERT INTO exec_snapshots (exec_key, rev, root_ref, commit_id, written_epoch)
             VALUES (?1, 1, ?2, ?3, ?4)";

        /// Replace execution `?1`'s root at revision `?2` with `?3` by commit
        /// `?4` at epoch `?5`, clearing any inline snapshot it replaces. No
        /// row when the stored revision is not `?2`.
        exec_replace = "UPDATE exec_snapshots
             SET rev = rev + 1, root_ref = ?3, commit_id = ?4, written_epoch = ?5,
                 snapshot_ref = NULL, executable_identity = NULL, format_version = NULL
             WHERE exec_key = ?1 AND rev = ?2
             RETURNING rev";

        /// Retire execution `?1`'s root `?2`. No row when it is not `?2`.
        exec_retire = "DELETE FROM exec_snapshots WHERE exec_key = ?1 AND root_ref = ?2
             RETURNING exec_key";

        /// Append owner `?1`'s run `?2` fact `?3` of kind `?4` for call `?5`,
        /// RoundFact node `?6`, at epoch `?7`.
        fact_append = "INSERT INTO run_records
                 (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

        /// Owner `?1`'s run `?2` fact `?3`'s node.
        fact_root = "SELECT record_ref FROM run_records
             WHERE owner_key = ?1 AND run_seq = ?2 AND ordinal = ?3";

        /// Every graph fact of owner `?1`, by run and ordinal.
        facts = "SELECT run_seq, ordinal, kind, call_id, record_ref, written_epoch
             FROM run_records WHERE owner_key = ?1 AND record_ref IS NOT NULL
             ORDER BY run_seq, ordinal";

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

        /// Every admission of execution `?1`, by effect identity.
        admissions = "SELECT effect_id, owner_key, run_seq, member_ordinal, admission_ref,
                    written_epoch
             FROM effect_admissions WHERE exec_key = ?1 ORDER BY effect_id";

        /// Run `?2` of session `?1`'s phase root.
        phase_root = "SELECT checkpoint_ref FROM turn_phases WHERE session_id = ?1 AND run = ?2";

        /// Point run `?2` of session `?1`'s phase at `?3` when it is `?4`,
        /// clearing the inline checkpoint the graph root replaces. No row
        /// otherwise.
        phase_set = "UPDATE turn_phases SET checkpoint_ref = ?3, inline_checkpoint = NULL
             WHERE session_id = ?1 AND run = ?2 AND checkpoint_ref IS NOT DISTINCT FROM ?4
             RETURNING run";

        /// Run `?2` of session `?1`'s working execution root.
        working_root = "SELECT working_execution_ref FROM turn_phases
             WHERE session_id = ?1 AND run = ?2";

        /// Point run `?2` of session `?1`'s working execution at `?3` when it
        /// is `?4`. No row otherwise.
        working_set = "UPDATE turn_phases SET working_execution_ref = ?3
             WHERE session_id = ?1 AND run = ?2
               AND working_execution_ref IS NOT DISTINCT FROM ?4
             RETURNING run";

        /// Plugin `?3`'s namespace root for run `?2` of session `?1`.
        namespace_root = "SELECT graph_root_ref FROM turn_namespaces
             WHERE session_id = ?1 AND run = ?2 AND plugin = ?3";

        /// Point plugin `?3`'s namespace for run `?2` of session `?1` at `?4`
        /// when it is `?5`. No row otherwise, or when the namespace has no
        /// row.
        namespace_set = "UPDATE turn_namespaces SET graph_root_ref = ?4
             WHERE session_id = ?1 AND run = ?2 AND plugin = ?3
               AND graph_root_ref IS NOT DISTINCT FROM ?5
             RETURNING plugin";

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

        /// Insert read pin `?1` of `?2`, generation `?3`, on `?4`, expiring at
        /// `?5`.
        pin_insert = "INSERT INTO blob_read_pins
                 (pin_id, lease_owner, generation, root_ref, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Read pin `?1`'s root.
        pin_root = "SELECT root_ref FROM blob_read_pins WHERE pin_id = ?1";

        /// Renew read pin `?1` to the next generation, expiring at `?2`.
        pin_renew = "UPDATE blob_read_pins
             SET generation = generation + 1, expires_at_ms = ?2
             WHERE pin_id = ?1
             RETURNING generation";

        /// Retire read pin `?1` held by `?2` at generation `?3` on `?4`. No
        /// row when it is not exactly that pin.
        pin_retire = "DELETE FROM blob_read_pins
             WHERE pin_id = ?1 AND lease_owner = ?2 AND generation = ?3 AND root_ref = ?4
             RETURNING pin_id";

        /// Retire every read pin that expired before `?1`.
        pins_retire_expired = "DELETE FROM blob_read_pins WHERE expires_at_ms < ?1";

        /// Owner `?1`'s receipt of commit `?2`.
        receipt = "SELECT batch_digest, result_bytes FROM graph_commit_receipts
             WHERE owner_key = ?1 AND commit_id = ?2";

        /// Record owner `?1`'s commit `?2` of digest `?3` answering `?4` at
        /// epoch `?5`.
        receipt_insert = "INSERT INTO graph_commit_receipts
                 (owner_key, commit_id, batch_digest, result_bytes, written_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Drop owner `?1`'s receipts: its retry lifetime ended.
        receipts_retire = "DELETE FROM graph_commit_receipts WHERE owner_key = ?1";

        /// Every root of source `?1`, as the catalog names it.
        catalog_source = "SELECT root_ref FROM graph_root_catalog WHERE source = ?1";
    }
}
