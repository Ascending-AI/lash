use super::*;

pub(super) struct TestSessionStoreFactory;
pub(super) struct InlineSessionStoreFactory;
pub(super) struct SegmentBoundarySessionStoreFactory;

// These factories create attachment-aware stores but do not retain them, so
// they cannot enumerate roots and must fail closed if passed to GC.
#[async_trait::async_trait]
impl crate::AttachmentRootSet for TestSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for InlineSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for SegmentBoundarySessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "live_attachment_refs",
        })
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "has_live_attachment_ref",
        })
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for TestSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(Arc::new(InMemorySessionStore::default()))
    }

    // Stateless: every create_store hands back a fresh in-memory store and no
    // tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &str,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for InlineSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(Arc::new(InMemorySessionStore::default()))
    }

    // Stateless: every create_store hands back a fresh in-memory store and no
    // tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &str,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SegmentBoundarySessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(Arc::new(InMemorySessionStore::default()))
    }

    // Stateless: every create_store hands back a fresh in-memory store and no
    // tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &str,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }
}

#[tokio::test]
async fn attachment_untracked_process_worker_factories_fail_closed() {
    let id = crate::attachments::content_id(b"untracked-factory-root-probe");
    let factories: [(&str, &dyn crate::AttachmentRootSet); 3] = [
        ("test", &TestSessionStoreFactory),
        ("inline", &InlineSessionStoreFactory),
        ("segment-boundary", &SegmentBoundarySessionStoreFactory),
    ];

    for (name, factory) in factories {
        assert!(
            matches!(
                factory.live_attachment_refs(0).await,
                Err(crate::StoreError::UnsupportedStoreOperation {
                    operation: "live_attachment_refs"
                })
            ),
            "{name} factory must reject root enumeration"
        );
        assert!(
            matches!(
                factory.has_live_attachment_ref(&id, 0).await,
                Err(crate::StoreError::UnsupportedStoreOperation {
                    operation: "has_live_attachment_ref"
                })
            ),
            "{name} factory must reject targeted root probes"
        );
    }
}
