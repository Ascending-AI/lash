//! A session catalog must state its by-id lookup answer.
//!
//! `Absent` is a durable negative answer, not a fallback for a catalog that
//! cannot resolve an id. The missing method must remain a compile error.

use lash::{SessionId, SessionListFilter, SessionView};
use lash::persistence::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, MaintenanceResult, SessionAdmission,
    SessionBlobReclaimReport, SessionCatalogStore, SessionStoreCreateRequest, StoreError,
};

struct SilentLookup;

#[async_trait::async_trait]
impl SessionCatalogStore for SilentLookup {
    async fn admit_session(
        &self,
        _request: &SessionStoreCreateRequest,
    ) -> Result<SessionAdmission, StoreError> {
        unreachable!()
    }

    async fn list_sessions(
        &self,
        _filter: &SessionListFilter,
    ) -> Result<Vec<SessionView>, StoreError> {
        unreachable!()
    }

    async fn fork_session(
        &self,
        _request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError> {
        unreachable!()
    }

    async fn pin(&self, _node_id: &lash::NodeId) -> Result<ForkPoint, StoreError> {
        unreachable!()
    }

    async fn unpin(&self, _node_id: &lash::NodeId) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError> {
        unreachable!()
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> MaintenanceResult<SessionBlobReclaimReport> {
        unreachable!()
    }
}

fn main() {}
