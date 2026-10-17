use crate::*;
use std::sync::Arc;
/// The deployment's store: the multi-session [`RuntimeStore`](crate::store::RuntimeStore)
/// plus the operations that span the deployment's catalog (ADR 0112 §2).
///
/// A host, the facade and the engine hold `Arc<dyn DeploymentStore>`.
/// Runtime code holds a [`SessionStore`](crate::store::SessionStore), which
/// reaches the same object as an `Arc<dyn RuntimeStore>` by trait upcasting.
///
/// Every operation is required, with no default: a store states its answer,
/// and a decorator forwards to the store it wraps
/// ([`DeploymentStoreDecorator`](super::DeploymentStoreDecorator)).
#[async_trait::async_trait]
pub trait DeploymentStore:
    crate::store::RuntimeStore + crate::AttachmentRootSet + crate::store::ControlIntentStore
{
    /// Count the deployment's turns that are not settled yet: parked turns
    /// and every turn in flight (FIG-3586). `drain_status` reads it, so a
    /// deployment with a parked turn — or one whose claims a crashed driver
    /// still holds — never reports drained. A store that keeps no countable
    /// catalog returns `StoreError::UnsupportedStoreOperation` rather than
    /// report zero: an inferred zero would let a host retire a deployment
    /// with turns still in flight.
    /// Whether a committed frame remains rooted by a head, anchor or retained
    /// admission. Prepared frames that never committed answer false.
    async fn artifact_frame_is_retained(
        &self,
        frame: &crate::FrameEnvironmentId,
    ) -> Result<bool, crate::StoreError>;

    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError>;

    /// Durable turn and session terminals strictly after `after`, in commit
    /// order. Independent of live replay; pages return the retention horizon.
    /// A cursor behind that horizon refuses as `TurnChangeCursorPruned`.
    async fn turns_changed_since(
        &self,
        after: crate::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::TurnChangePage, crate::StoreError>;

    /// Retained intents, including permanent engine refusals, in ID order.
    async fn list_control_intents(
        &self,
        after: Option<crate::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::store::ControlIntent>, crate::StoreError>;

    /// Reclaim deployment-wide evidence before an explicit host horizon
    /// (FIG-653).
    ///
    /// Receipts require a durably deleted owning session. Usage deltas require
    /// that same terminal marker and absence of their receipt after the sweep;
    /// live-session ledgers and receipts are never eligible. Receipt deletion
    /// and dependent attachment/usage reconciliation share one transaction fence.
    /// A retry in a deleted scope returns `StoreError::SessionDeleted`, before
    /// and after receipt pruning. The permanent identity tombstone is exempt.
    /// No daemon, clock read or live policy lookup runs this operation.
    ///
    /// The host tool-intent submission ledger is evidence under it too
    /// (FIG-1509): a row admitted before the bound is reclaimed once its owner
    /// session is durably deleted, and that owner is fenced in the same
    /// transaction, so a resubmission is refused rather than realized again.
    ///
    /// SQL stores reclaim their retained storage evidence here. This sweep
    /// does not retire engine scopes or decide whether an execution is live.
    async fn reclaim_retained_evidence(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport>;
}

/// Whether the catalog holds `session_id` live: durable metadata and no
/// deletion tombstone. A catalog that cannot answer is `Err`, never `false`.
pub async fn session_is_live(
    store: &dyn crate::store::RuntimeStore,
    session_id: &SessionId,
) -> Result<bool, crate::StoreError> {
    Ok(matches!(
        store.lookup_session(session_id).await?,
        crate::store::SessionLookup::Live(_)
    ))
}

/// The view of `session_id` on `store` when the catalog holds it live, and
/// `None` when it is absent or deleted (ADR 0112 §2: `lookup_session` plus
/// `SessionStore::new`). A catalog that cannot answer is `Err`.
pub async fn live_session_view(
    store: &Arc<dyn crate::DeploymentStore>,
    session_id: &SessionId,
) -> Result<Option<crate::store::SessionStore>, crate::StoreError> {
    match store.lookup_session(session_id).await? {
        crate::store::SessionLookup::Live(_) => {
            let runtime: Arc<dyn crate::store::RuntimeStore> = store.clone();
            crate::store::SessionStore::new(runtime, session_id.clone()).map(Some)
        }
        crate::store::SessionLookup::Deleted | crate::store::SessionLookup::Absent => Ok(None),
    }
}

/// Admit `request`'s session on the catalog and return its view (ADR 0112
/// §2: `admit_session` plus `SessionStore::new`).
pub async fn admit_session_view(
    store: &Arc<dyn crate::DeploymentStore>,
    request: &crate::SessionStoreCreateRequest,
) -> Result<crate::store::SessionStore, crate::StoreError> {
    store.admit_session(request).await?;
    let runtime: Arc<dyn crate::store::RuntimeStore> = store.clone();
    crate::store::SessionStore::new(runtime, request.session_id.clone())
}

/// The session-state generation gate for work that acts for a session from
/// outside that session's execution lease (FIG-3619).
///
/// A turn meets the generation fence when it claims its lane
/// ([`SessionCommitStore::admit_session_state`](crate::store::SessionCommitStore::admit_session_state)).
/// Work a durable engine runs as a separate invocation on the session's
/// behalf, such as a process segment, may be routed to a newer
/// deployment than the turn that opened it, and never claims that lane. It
/// calls this at invocation entry, before it reads a journal or derives an
/// effect key: the owning session's physical marker is classified by the
/// same backend read the lease admission uses, so both paths refuse exactly
/// the same generations and a generation bump moves them together.
///
/// Refusal is `Err(StoreError::SessionStateVersionUnsupported)` or
/// `Err(StoreError::SessionStateVersionNewerThanRuntime)`, with the found
/// generation. A session the catalog does not hold, or one that was deleted,
/// passes: there is no generation to refuse, and the deletion fences own that
/// work's fate. Any other error is the catalog's or the store's, and the
/// caller decides whether to retry it.
pub async fn admit_session_state_generation(
    store: &dyn crate::store::RuntimeStore,
    session_id: &SessionId,
) -> Result<(), crate::StoreError> {
    if !session_is_live(store, session_id).await? {
        return Ok(());
    }
    match store.read_session_state_version(session_id).await {
        Ok(_) | Err(crate::StoreError::SessionDeleted { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}
