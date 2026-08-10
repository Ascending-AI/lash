use std::sync::Arc;

use lash::persistence::{
    AttachmentReclamationPolicy, EmptyRootSetPolicy, InMemoryAttachmentStore,
    InMemorySessionStoreFactory, RuntimePersistence, SessionStoreCreateRequest,
    SessionStoreFactory, StoreError, reclaim_unreferenced_attachments,
};

struct DelegatingFactory {
    inner: InMemorySessionStoreFactory,
}

#[async_trait::async_trait]
impl SessionStoreFactory for DelegatingFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        self.inner.open_existing_store(request).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        self.inner.delete_session(session_id).await
    }
}

async fn try_gc(factory: &DelegatingFactory) {
    let backend = InMemoryAttachmentStore::new();
    let _ = reclaim_unreferenced_attachments(
        factory,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    );
}

fn main() {}
