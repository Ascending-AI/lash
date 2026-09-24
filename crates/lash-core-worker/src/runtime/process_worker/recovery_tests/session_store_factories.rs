use super::*;

/// A catalog that creates but cannot resolve a session by id: its
/// `open_existing_store_by_id` answer is the typed no-lookup refusal, and
/// every store it creates is the backend catalog's. Session initialisation
/// must fail the process on the first admission — the refusal is a
/// capability fact no retry can change, so re-admitting would only livelock
/// the worker (FIG-3487).
pub(super) struct NoByIdLookupSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    by_id_opens: AtomicUsize,
}

impl NoByIdLookupSessionStoreFactory {
    pub(super) fn over(inner: Arc<dyn SessionStoreFactory>) -> Self {
        Self {
            inner,
            by_id_opens: AtomicUsize::new(0),
        }
    }

    pub(super) fn by_id_opens(&self) -> usize {
        self.by_id_opens.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for NoByIdLookupSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for NoByIdLookupSessionStoreFactory {
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        self.inner.create_store(request).await
    }

    // The fixture's one defect is the point: no non-creating by-id seam,
    // stated as the typed capability refusal.
    async fn open_existing_store_by_id(
        &self,
        _session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        self.by_id_opens.fetch_add(1, Ordering::SeqCst);
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::open_existing_store_by_id",
        })
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        self.inner.count_unsettled_turns().await
    }
}
