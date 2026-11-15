//! `artifact_dependency_edges`: what an artifact itself keeps alive
//! (FIG-7700).
//!
//! An environment head holds its body, and a body holds every module of its
//! closure. A holder is not a referrer: these rows never enter the referrer
//! vocabulary, fences, cleanup obligations or any authority answer. They
//! count only for retention. An artifact is unreferenced when no referrer
//! edge and no dependency edge names it.
//!
//! A holder's edges are written with the holder, in its publish transaction,
//! and are immutable while it lives: deleting the holder's row deletes them
//! (the holder foreign key cascades), and the deleter reclaims the targets
//! that frees in the same transaction. Authority readers ask what a referrer
//! reaches: its own edges, then what those artifacts hold, transitively.

/// The table's unprefixed name.
pub const TABLE: &str = "artifact_dependency_edges";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "namespace, artifact_ref, holder_namespace, holder_ref";

crate::statements! {
    /// `artifact_dependency_edges` statements both backends issue verbatim.
    pub struct DependencyEdgeStatements @ "artifact_dependency_edge" {
        /// Holder `?3`/`?4` keeps artifact `?1`/`?2`.
        insert_edge = "INSERT INTO artifact_dependency_edges
             (namespace, artifact_ref, holder_namespace, holder_ref)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING";

        /// Whether referrer `?2`/`?3` reaches any artifact of namespace `?1`:
        /// through its own edges, or through what those artifacts hold.
        select_reaches_namespace = "WITH RECURSIVE reach AS (
                 SELECT namespace, artifact_ref FROM artifact_referrer_edges
                 WHERE referrer_kind = ?2 AND referrer_id = ?3
                 UNION
                 SELECT held.namespace, held.artifact_ref
                 FROM artifact_dependency_edges AS held
                 JOIN reach ON held.holder_namespace = reach.namespace
                     AND held.holder_ref = reach.artifact_ref
             ) SELECT EXISTS (SELECT 1 FROM reach WHERE namespace = ?1)";

        /// Whether referrer `?2`/`?3` reaches artifact `?4` of namespace `?1`,
        /// as [`Self::select_reaches_namespace`] reaches.
        select_reaches = "WITH RECURSIVE reach AS (
                 SELECT namespace, artifact_ref FROM artifact_referrer_edges
                 WHERE referrer_kind = ?2 AND referrer_id = ?3
                 UNION
                 SELECT held.namespace, held.artifact_ref
                 FROM artifact_dependency_edges AS held
                 JOIN reach ON held.holder_namespace = reach.namespace
                     AND held.holder_ref = reach.artifact_ref
             ) SELECT EXISTS (SELECT 1 FROM reach WHERE namespace = ?1 AND artifact_ref = ?4)";
    }
}
