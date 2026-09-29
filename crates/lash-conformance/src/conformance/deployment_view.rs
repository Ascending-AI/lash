//! Test fixture construction over the multi-session deployment catalog.

use std::sync::Arc;

use crate::store::{ConformanceDeployment, SessionLookup, SessionStore};
use crate::{DeploymentStore, RuntimeStore, SessionId, SessionStoreCreateRequest, StoreError};

#[async_trait::async_trait]
pub(super) trait DeploymentViewExt {
    async fn admit_view(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<SessionStore, StoreError>;

    async fn live_view(&self, session_id: &SessionId) -> Result<Option<SessionStore>, StoreError>;

    async fn live_view_for(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<SessionStore>, StoreError> {
        self.live_view(&request.session_id).await
    }

    async fn read_view(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::SessionReadView>, StoreError> {
        let Some(view) = self.live_view(session_id).await? else {
            return Ok(None);
        };
        crate::store::load_session_read_view(&view).await
    }

    async fn is_deleted(&self, session_id: &SessionId) -> Result<bool, StoreError>;
}

macro_rules! impl_deployment_view {
    ($ty:ty) => {
        #[async_trait::async_trait]
        impl DeploymentViewExt for Arc<$ty> {
            async fn admit_view(
                &self,
                request: &SessionStoreCreateRequest,
            ) -> Result<SessionStore, StoreError> {
                self.admit_session(request).await?;
                let runtime: Arc<dyn RuntimeStore> = self.clone();
                SessionStore::new(runtime, request.session_id.clone())
            }

            async fn live_view(
                &self,
                session_id: &SessionId,
            ) -> Result<Option<SessionStore>, StoreError> {
                match self.lookup_session(session_id).await? {
                    SessionLookup::Live(_) => {
                        let runtime: Arc<dyn RuntimeStore> = self.clone();
                        SessionStore::new(runtime, session_id.clone()).map(Some)
                    }
                    SessionLookup::Deleted | SessionLookup::Absent => Ok(None),
                }
            }

            async fn is_deleted(&self, session_id: &SessionId) -> Result<bool, StoreError> {
                Ok(matches!(
                    self.lookup_session(session_id).await?,
                    SessionLookup::Deleted
                ))
            }
        }
    };
}

impl_deployment_view!(dyn DeploymentStore);
impl_deployment_view!(dyn ConformanceDeployment);
