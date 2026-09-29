use std::sync::Arc;

use lash_core::{DeploymentStore, RuntimeStore, SessionId, SessionStoreCreateRequest, StoreError};

pub(super) async fn admit_test_session(
    factory: Arc<dyn DeploymentStore>,
    request: &SessionStoreCreateRequest,
) -> Result<Arc<dyn RuntimeStore>, StoreError> {
    factory.admit_session(request).await?;
    let runtime: Arc<dyn RuntimeStore> = factory;
    Ok(runtime)
}

pub(super) async fn look_up_test_session(
    factory: Arc<dyn DeploymentStore>,
    session_id: &SessionId,
) -> Result<Option<Arc<dyn RuntimeStore>>, StoreError> {
    match factory.lookup_session(session_id).await? {
        lash_core::SessionLookup::Live(_) => {
            let runtime: Arc<dyn RuntimeStore> = factory;
            Ok(Some(runtime))
        }
        lash_core::SessionLookup::Absent | lash_core::SessionLookup::Deleted => Ok(None),
    }
}
