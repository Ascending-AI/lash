//! A `SessionStoreFactory` that never states its tombstone answer must not
//! compile.
//!
//! `session_was_deleted` decides whether a resume hands back the caller's
//! conversation or a fresh empty one under a dead id. An inherited "no
//! tombstone here" is a claim the implementor never made, so the method is
//! required: omitting it is a compile error, not a default.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash::attachments::AttachmentId;
use lash::persistence::{
    AttachmentRootSet, InMemorySessionStoreFactory, RuntimePersistence, SessionStoreCreateRequest,
    SessionStoreFactory, StoreError,
};

struct SilentFactory {
    inner: InMemorySessionStoreFactory,
}

#[async_trait::async_trait]
impl AttachmentRootSet for SilentFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SilentFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        self.inner.delete_session(session_id).await
    }
}

fn main() {}
