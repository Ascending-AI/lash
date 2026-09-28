//! `artifact_owners`: the exact owner edges of an immutable artifact.
//!
//! One row per (namespace, artifact, owner) edge. The edge is the liveness
//! fact: there is no maintained reference count and no last-operation stamp on
//! shared bytes, so an artifact is reclaimable exactly when this table holds
//! no edge for it.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_owners";

/// Every column, in insert order. The whole row is its primary key.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, owner_kind, owner_id";

crate::statements! {
    /// `artifact_owners` statements both backends issue verbatim.
    pub struct OwnerStatements @ "artifact_owner" {
        /// Owning an artifact twice is the same fact as owning it once.
        insert_edge = "INSERT INTO artifact_owners
             (namespace, artifact_ref, owner_kind, owner_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING";

        select_edge_exists = "SELECT EXISTS (
                 SELECT 1 FROM artifact_owners
                 WHERE namespace = ?1 AND artifact_ref = ?2
                   AND owner_kind = ?3 AND owner_id = ?4
             )";

        /// Sever the one edge `?3`/`?4` holds over artifact `?1`/`?2`.
        delete_edge = "DELETE FROM artifact_owners
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND owner_kind = ?3 AND owner_id = ?4";

        /// Sever every edge owner `?2`/`?3` holds in namespace `?1`: the
        /// retirement sweep's second step, after the permanent fence is
        /// recorded.
        delete_owner_edges = "DELETE FROM artifact_owners
             WHERE namespace = ?1 AND owner_kind = ?2 AND owner_id = ?3";
    }
}
