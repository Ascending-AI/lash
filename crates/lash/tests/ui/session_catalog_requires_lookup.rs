//! A session catalog must state its by-id lookup answer.
//!
//! `Absent` is a durable negative answer, not a fallback for a catalog that
//! cannot resolve an id. The missing method must remain a compile error.

use lash::{SessionId, SessionListFilter, SessionView};
use lash::persistence::{
    ForkSessionReceipt, ForkSessionRequest, MaintenanceResult, RetainedRevision, Retention,
    SessionAdmission, SessionBlobReclaimReport, SessionCatalogStore, SessionStoreCreateRequest,
    StoreError, Target,
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

    async fn resolve_target(
        &self,
        _session_id: &SessionId,
        _target: &Target,
    ) -> Result<RetainedRevision, StoreError> {
        unreachable!()
    }

    async fn revisions(&self, _session_id: &SessionId) -> Result<Vec<RetainedRevision>, StoreError> {
        unreachable!()
    }

    async fn pin(&self, _session_id: &SessionId, _target: &Target) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn unpin(&self, _session_id: &SessionId, _target: &Target) -> Result<(), StoreError> {
        unreachable!()
    }

    async fn retention(&self, _session_id: &SessionId) -> Result<Retention, StoreError> {
        unreachable!()
    }

    async fn set_retention(
        &self,
        _session_id: &SessionId,
        _retention: Retention,
    ) -> Result<(), StoreError> {
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
