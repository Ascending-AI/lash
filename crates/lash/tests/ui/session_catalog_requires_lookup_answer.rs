//! A session catalog that never states its lookup answer must not compile.
//!
//! `lookup_session` decides whether a resume hands back the caller's
//! conversation, refuses a deleted one, or starts fresh under an unknown id
//! (ADR 0112). An inherited "absent" is a claim the implementor never made, so
//! the operation is required: omitting it is a compile error, not a default.

use std::sync::Arc;

use lash::persistence::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, MaintenanceResult, RuntimeStore,
    SessionAdmission, SessionBlobReclaimReport, SessionCatalogStore, SessionStoreCreateRequest,
    StoreError,
};
use lash::{NodeId, SessionId, SessionListFilter, SessionSummary};

struct SilentCatalog {
    inner: Arc<dyn RuntimeStore>,
}

#[async_trait::async_trait]
impl SessionCatalogStore for SilentCatalog {
    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<SessionAdmission, StoreError> {
        self.inner.admit_session(request).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError> {
        self.inner.list_sessions(filter).await
    }

    async fn fork_session(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError> {
        self.inner.fork_session(request).await
    }

    async fn pin(&self, node_id: &NodeId) -> Result<ForkPoint, StoreError> {
        self.inner.pin(node_id).await
    }

    async fn unpin(&self, node_id: &NodeId) -> Result<(), StoreError> {
        self.inner.unpin(node_id).await
    }

    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError> {
        self.inner.fork_points().await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> MaintenanceResult<SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }
}

fn main() {}
