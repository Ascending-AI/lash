//! The catalog segment of the multi-session store (ADR 0112 §1.1).
//!
//! One store object serves every session of its catalog. These operations
//! create, find, enumerate, fork and delete sessions; each one either takes
//! the session it acts on or spans the catalog, and says which.
use super::{
    CommitBudget, FleetFormat, MaintenanceResult, RuntimeCommit, SessionAdmission,
    SessionBlobReclaimReport, StoreError,
};
use crate::SessionId;
use crate::session_catalog::{SessionListFilter, SessionView};
use crate::session_store_factory_types::{
    ForkSessionReceipt, ForkSessionRequest, RetainedRevision, Retention, SessionLookup,
    SessionStoreCreateRequest, Target,
};

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
    ) -> Result<Vec<SessionView>, StoreError>;

    /// Add a new session head at a retained revision without writing graph
    /// nodes. `request.session_id` names the new session, and
    /// `(request.source_session_id, request.head_revision)` names the state
    /// it starts at. The fork gets fresh session and execution identities and
    /// copies no pending ingress, pin or retention policy.
    ///
    /// A revision the source no longer retains refuses
    /// [`StoreError::ForkTargetPruned`]; no other revision is substituted.
    async fn fork_session(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError>;

    /// The retained revision `target` of `session_id` names.
    ///
    /// The answer is computed by query and stored nowhere:
    ///
    /// * [`StoreError::ForkTargetPending`]: the target's root has not
    ///   finished, or nothing has recorded the target yet;
    /// * [`StoreError::ForkTargetUnavailable`]: the root ended without a
    ///   commit, or the input was withdrawn;
    /// * [`StoreError::ForkTargetPruned`]: the revision was collected.
    ///
    /// A refusal never answers another revision in the target's place.
    async fn resolve_target(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<RetainedRevision, StoreError>;

    /// Every retained revision of `session_id`, oldest first: the points it
    /// can be forked at.
    async fn revisions(&self, session_id: &SessionId) -> Result<Vec<RetainedRevision>, StoreError>;

    /// Pin `target` of `session_id`: the revision it resolves to is retained
    /// through every collection until it is unpinned or its session is
    /// deleted.
    ///
    /// The write is idempotent and names only the target. It may precede the
    /// target, run beside it or follow it; it takes no execution authority
    /// and never reads or moves the head. A session the catalog does not
    /// hold refuses [`StoreError::SessionNotFound`].
    async fn pin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError>;

    /// Release the pin on `target` of `session_id`. Releasing a pin that is
    /// not there changes nothing. The revision stays retained while another
    /// pin, the head or the retention policy holds it.
    async fn unpin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError>;

    /// The retention policy of `session_id`.
    async fn retention(&self, session_id: &SessionId) -> Result<Retention, StoreError>;

    /// Set the retention policy of `session_id`. It takes effect at the
    /// session's next commit and at the host's next collection.
    async fn set_retention(
        &self,
        session_id: &SessionId,
        retention: Retention,
    ) -> Result<(), StoreError>;

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

/// Create a session through `catalog` under the host's `commit_budget`
/// (FIG-4393).
///
/// A creating admission bakes the creator's config into the head
/// ([`SessionCreationHead::Config`]): the store writes that head in the
/// catalog's own transaction, outside any runtime commit, so the budget is
/// checked here first. A config whose created head no commit fits under the
/// budget, not even a session command's bare settlement over it, is refused
/// with the typed [`StoreError::CommitByteBudgetExceeded`] or
/// [`StoreError::CommitNodeBudgetExceeded`] the first commit would meet, and
/// nothing is written.
///
/// # Errors
///
/// The budget refusal, or whatever [`SessionCatalogStore::admit_session`]
/// answers.
pub async fn admit_created_session(
    catalog: &(dyn SessionCatalogStore + '_),
    request: &SessionStoreCreateRequest,
    commit_budget: CommitBudget,
    fleet_format: FleetFormat,
) -> Result<SessionAdmission, StoreError> {
    RuntimeCommit::validate_created_head_budget(
        &request.session_id,
        request.config.clone(),
        commit_budget,
        fleet_format,
    )?;
    catalog.admit_session(request).await
}
