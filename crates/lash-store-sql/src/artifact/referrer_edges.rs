//! `artifact_referrer_edges`: the exact referrer edges of an immutable
//! artifact (ADR 0113 §1).
//!
//! One row per (namespace, artifact, referrer) edge. The edge is the liveness
//! fact: there is no maintained reference count and no last-operation stamp on
//! shared bytes, so an artifact is reclaimable exactly when this table holds
//! no edge for it. `(referrer_kind, referrer_id)` is the referrer's stored
//! pair; every read decodes it through `ArtifactReferrer::decode`, and a
//! refusal is `StoredDataCorrupt`, never a skipped row.
//!
//! The foreign key names the backend's byte table: SQLite's `artifact_refs`,
//! PostgreSQL's `lash_lashlang_artifacts`.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_referrer_edges";

/// Every column, in insert order. The whole row is its primary key.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, referrer_kind, referrer_id";

/// The columns a referrer's edge listing reads, in its order.
pub const REFERRER_EDGE_COLUMNS: &str = "namespace, artifact_ref";

/// The columns an artifact's edge listing reads, in its order.
pub const ARTIFACT_EDGE_COLUMNS: &str = "referrer_kind, referrer_id";

crate::statements! {
    /// `artifact_referrer_edges` statements both backends issue verbatim.
    pub struct ReferrerEdgeStatements @ "artifact_referrer_edge" {
        /// Referring to an artifact twice is the same fact as referring to it
        /// once.
        insert_edge = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING";

        select_edge_exists = "SELECT EXISTS (
                 SELECT 1 FROM artifact_referrer_edges
                 WHERE namespace = ?1 AND artifact_ref = ?2
                   AND referrer_kind = ?3 AND referrer_id = ?4
             )";

        /// Every edge referrer `?1`/`?2` holds, in every namespace, in
        /// `(namespace, artifact_ref)` order: what an end-referrer severs and
        /// then reclaims.
        select_referrer_edges = "SELECT namespace, artifact_ref FROM artifact_referrer_edges
             WHERE referrer_kind = ?1 AND referrer_id = ?2
             ORDER BY namespace, artifact_ref";

        /// Every edge referrer `?2`/`?3` holds in namespace `?1`, in
        /// `artifact_ref` order: one store's share of an end-referrer.
        select_referrer_edges_in_namespace = "SELECT namespace, artifact_ref
             FROM artifact_referrer_edges
             WHERE namespace = ?1 AND referrer_kind = ?2 AND referrer_id = ?3
             ORDER BY artifact_ref";

        /// Every referrer of artifact `?1`/`?2`, in stored-pair order.
        select_artifact_edges = "SELECT referrer_kind, referrer_id FROM artifact_referrer_edges
             WHERE namespace = ?1 AND artifact_ref = ?2
             ORDER BY referrer_kind, referrer_id";

        /// Whether any referrer still holds artifact `?1`/`?2`.
        select_artifact_has_edge = "SELECT EXISTS (
                 SELECT 1 FROM artifact_referrer_edges
                 WHERE namespace = ?1 AND artifact_ref = ?2
             )";

        /// Sever the one edge `?3`/`?4` holds over artifact `?1`/`?2`.
        delete_edge = "DELETE FROM artifact_referrer_edges
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND referrer_kind = ?3 AND referrer_id = ?4";

        /// Sever every edge referrer `?2`/`?3` holds in namespace `?1`: an
        /// end-referrer's third step, after its fence and carries.
        delete_referrer_edges_in_namespace = "DELETE FROM artifact_referrer_edges
             WHERE namespace = ?1 AND referrer_kind = ?2 AND referrer_id = ?3";

        /// Give referrer `?3`/`?4` an edge on every artifact referrer
        /// `?1`/`?2` holds: a fork of a live frame copies the ancestor frame's
        /// edges to its own.
        copy_referrer_edges = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             SELECT namespace, artifact_ref, ?3, ?4 FROM artifact_referrer_edges
             WHERE referrer_kind = ?1 AND referrer_id = ?2
             ON CONFLICT DO NOTHING";
    }
}
