use super::*;

/// The session catalog these factories keep: `create_store` binds one
/// `InMemorySessionStore` per session id and by-id opens resolve exactly the
/// ids a create bound. Session initialisation inspects the catalog on every
/// `ProcessInput::SessionTurn` run, so a factory that cannot answer the by-id
/// seam cannot run a session turn at all.
#[derive(Default)]
struct SessionStoreCatalog {
    stores: Mutex<std::collections::HashMap<SessionId, Arc<InMemorySessionStore>>>,
}

impl SessionStoreCatalog {
    fn create(&self, session_id: &SessionId) -> Arc<InMemorySessionStore> {
        Arc::clone(
            self.stores
                .lock_recover()
                .entry(session_id.clone())
                .or_default(),
        )
    }

    /// `None` for an id no create bound — "no such session" stays a real
    /// answer; the catalog never auto-vivifies a row on lookup.
    fn by_id(&self, session_id: &SessionId) -> Option<Arc<InMemorySessionStore>> {
        self.stores.lock_recover().get(session_id).cloned()
    }
}

#[derive(Default)]
pub(super) struct TestSessionStoreFactory {
    catalog: SessionStoreCatalog,
}
#[derive(Default)]
pub(super) struct InMemorySessionStoreFactory {
    catalog: SessionStoreCatalog,
}
#[derive(Default)]
pub(super) struct SegmentBoundarySessionStoreFactory {
    catalog: SessionStoreCatalog,
}

/// A catalog that creates but cannot resolve a session by id: its
/// `open_existing_store_by_id` answer is the typed no-lookup refusal. Session
/// initialisation must fail the process on the first admission — the refusal
/// is a capability fact no retry can change, so re-admitting would only
/// livelock the worker (FIG-3487).
#[derive(Default)]
pub(super) struct NoByIdLookupSessionStoreFactory {
    by_id_opens: AtomicUsize,
}

impl NoByIdLookupSessionStoreFactory {
    pub(super) fn by_id_opens(&self) -> usize {
        self.by_id_opens.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for NoByIdLookupSessionStoreFactory {
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
impl SessionStoreFactory for NoByIdLookupSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(Arc::new(InMemorySessionStore::default()))
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

    // No tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }
}

// These factories keep a session catalog but no attachment-root index, so
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
impl crate::AttachmentRootSet for InMemorySessionStoreFactory {
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
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.catalog.create(&request.session_id))
    }

    async fn open_existing_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, String> {
        Ok(self
            .catalog
            .by_id(&request.session_id)
            .map(|store| store as _))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        Ok(self.catalog.by_id(session_id).map(|store| store as _))
    }

    // No tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for InMemorySessionStoreFactory {
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.catalog.create(&request.session_id))
    }

    async fn open_existing_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, String> {
        Ok(self
            .catalog
            .by_id(&request.session_id)
            .map(|store| store as _))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        Ok(self.catalog.by_id(session_id).map(|store| store as _))
    }

    // No tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SegmentBoundarySessionStoreFactory {
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.catalog.create(&request.session_id))
    }

    async fn open_existing_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, String> {
        Ok(self
            .catalog
            .by_id(&request.session_id)
            .map(|store| store as _))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        Ok(self.catalog.by_id(session_id).map(|store| store as _))
    }

    // No tombstone is ever recorded, so no session has been deleted.
    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }
}

#[tokio::test]
async fn attachment_untracked_process_worker_factories_fail_closed() {
    let id = crate::attachments::content_id(b"untracked-factory-root-probe");
    let factories: [(&str, &dyn crate::AttachmentRootSet); 3] = [
        ("test", &TestSessionStoreFactory::default()),
        ("in-memory", &InMemorySessionStoreFactory::default()),
        (
            "segment-boundary",
            &SegmentBoundarySessionStoreFactory::default(),
        ),
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
