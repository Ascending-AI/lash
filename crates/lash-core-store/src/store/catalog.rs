//! The catalog segment of the multi-session store (ADR 0112 §1.1).
//!
//! One store object serves every session of its catalog. These operations
//! create, find, enumerate, fork and delete sessions; each one either takes
//! the session it acts on or spans the catalog, and says which.
use super::{MaintenanceResult, SessionAdmission, SessionBlobReclaimReport, StoreError};
use crate::session_catalog::{SessionListFilter, SessionSummary};
use crate::session_store_factory_types::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, SessionLookup, SessionStoreCreateRequest,
};
use crate::{NodeId, SessionId};

/// Session admission, lookup, enumeration, forks and deletion.
///
/// Every operation is required: a store states its answer, and a decorator
/// forwards to the store it wraps.
#[async_trait::async_trait]
pub trait SessionCatalogStore: Send + Sync {
    /// The one admission seam. In one transaction, in this order:
    ///
    /// 1. refuse an invalid id with [`StoreError::InvalidSessionId`];
    /// 2. refuse a deletion tombstone with [`StoreError::SessionDeleted`]
    ///    (the FIG-1282 ordering, on every backend);
    /// 3. when no metadata exists, insert it exactly from the request
    ///    (`relation`, `pending_observer_intents`, `owning_process_id`) and
    ///    answer [`SessionAdmission::Created`];
    /// 4. otherwise check the recorded lineage with
    ///    [`guard_rebind_lineage`](crate::store_backend_support::guard_rebind_lineage):
    ///    a conflict is [`StoreError::SessionRelationMismatch`], and anything
    ///    else answers [`SessionAdmission::Rebound`] and leaves the row
    ///    untouched. [`SessionRelation::Root`](crate::SessionRelation::Root)
    ///    declares no lineage, so it always rebinds.
    ///
    /// Nothing is bound: the store holds no per-session handle state.
    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<SessionAdmission, StoreError>;

    /// What the catalog holds for `session_id`, without writing. A catalog
    /// that cannot answer returns `Err`, never [`SessionLookup::Absent`].
    async fn lookup_session(&self, session_id: &SessionId) -> Result<SessionLookup, StoreError>;

    /// Catalog-wide: durable catalog rows, without opening a session or
    /// acquiring execution authority. Ordered by `created_at_ms`, then
    /// `session_id`. Permanent deletion tombstones stay visible with
    /// `deleted == true`.
    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError>;

    /// Add a new session head at a retained point without writing graph
    /// nodes. `request.session_id` names the new session.
    async fn fork_session(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError>;

    /// Catalog-wide: retain the continuation checkpoint for `node_id`.
    ///
    /// A new pin can be created only while some live head is exactly at the
    /// node, because an unpinned past checkpoint is ordinarily already
    /// collectible. Re-pinning an existing point is idempotent.
    async fn pin(&self, node_id: &NodeId) -> Result<ForkPoint, StoreError>;

    /// Catalog-wide: release an explicit continuation pin. A live head at
    /// the same node keeps that tip forkable.
    async fn unpin(&self, node_id: &NodeId) -> Result<(), StoreError>;

    /// Catalog-wide: every retained continuation point, pinned past turns
    /// and unpinned live tips, de-duplicated by node id.
    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError>;

    /// Delete `session_id` and reclaim blobs whose final exact reference edge
    /// that transaction severs.
    ///
    /// Failure carries the partial report accumulated before the transaction
    /// rolled back, so a zero success report means witnessed emptiness,
    /// never an unreported reclaim failure.
    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> MaintenanceResult<SessionBlobReclaimReport>;
}
