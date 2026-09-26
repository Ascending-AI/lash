//! A `SessionStoreFactory` that never states its unsettled-turn count must
//! not compile.
//!
//! `LashCore::drain_status` counts the deployment's parked and in-flight turns
//! through `count_unsettled_turns`. An inherited refusal fails the first drain
//! at runtime, and an inherited zero would let a host retire a deployment
//! with turns still in flight, so the method is required: a decorator
//! forwards to the catalog it wraps, and a factory with no countable catalog
//! returns the typed refusal itself.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash::SessionId;
use lash::attachments::AttachmentId;
use lash::persistence::{
    AttachmentRootSet, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory,
    StoreError,
};

struct UncountedFactory {
    inner: Arc<dyn SessionStoreFactory>,
}

#[async_trait::async_trait]
impl AttachmentRootSet for UncountedFactory {
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
impl SessionStoreFactory for UncountedFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        self.inner.open_existing_store_by_id(session_id).await
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        SessionStoreFactory::session_was_deleted(self.inner.as_ref(), session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash::persistence::MaintenanceResult<lash::persistence::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }
}

#[async_trait::async_trait]
impl lash::persistence::ControlIntentStore for UncountedFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash::persistence::ControlIntent>, StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash::persistence::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash::persistence::IntentApplication, StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash::persistence::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<(), StoreError> {
        self.inner.acknowledge_intent(id, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash::persistence::ControlIntentId,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<lash::persistence::ControlIntent, StoreError> {
        self.inner.record_intent_failure(id, error, retryable, at_ms).await
    }

    async fn load_intent(
        &self,
        id: lash::persistence::ControlIntentId,
    ) -> std::result::Result<Option<lash::persistence::ControlIntent>, StoreError> {
        self.inner.load_intent(id).await
    }
}

fn main() {}
