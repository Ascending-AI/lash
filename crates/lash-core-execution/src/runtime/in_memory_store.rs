//! Public in-memory `RuntimePersistence` + `SessionStoreFactory`.
//!
//! The implementation lives in `lash-core-memory`; this module re-exports it at
//! the original path and adapts the concrete factory to the core-owned
//! `SessionStoreFactory` trait (which names `crate::EffectHost` and cannot move
//! down with the backend).
pub use lash_core_memory::in_memory_store::*;

#[async_trait::async_trait]
impl crate::SessionStoreFactory for InMemorySessionStoreFactory {
    async fn reclaim_retained_evidence(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport> {
        InMemorySessionStoreFactory::reclaim_retained_evidence(self, bound).await
    }
    async fn create_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<std::sync::Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        InMemorySessionStoreFactory::create_store(self, request).await
    }
    async fn open_existing_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<std::sync::Arc<dyn crate::RuntimePersistence>>, String> {
        InMemorySessionStoreFactory::open_existing_store(self, request).await
    }
    async fn read_session(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Option<crate::SessionReadView>, crate::StoreError> {
        InMemorySessionStoreFactory::read_session(self, session_id).await
    }
    async fn list_sessions(
        &self,
        filter: &crate::SessionListFilter,
    ) -> Result<Vec<crate::SessionSummary>, crate::StoreError> {
        InMemorySessionStoreFactory::list_sessions(self, filter).await
    }
    async fn open_existing_store_by_id(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Option<std::sync::Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
        InMemorySessionStoreFactory::open_existing_store_by_id(self, session_id).await
    }
    async fn count_unsettled_turns(
        &self,
    ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
        InMemorySessionStoreFactory::count_unsettled_turns(self).await
    }
    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, crate::StoreError> {
        InMemorySessionStoreFactory::pending_turn_cancel_closure_pins(self, session_id).await
    }
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<(), crate::StoreError> {
        InMemorySessionStoreFactory::retire_turn_cancel_closure_scope(self, scope).await
    }
    async fn has_claimable_queued_work(
        &self,
        request: &crate::SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, crate::StoreError> {
        InMemorySessionStoreFactory::has_claimable_queued_work(self, request, now_epoch_ms).await
    }
    async fn session_was_deleted(&self, session_id: &crate::SessionId) -> Result<bool, String> {
        InMemorySessionStoreFactory::session_was_deleted(self, session_id).await
    }
    async fn delete_session(
        &self,
        session_id: &crate::SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        InMemorySessionStoreFactory::delete_session(self, session_id).await
    }
    async fn pin(&self, node_id: &str) -> Result<crate::ForkPoint, crate::StoreError> {
        InMemorySessionStoreFactory::pin(self, node_id).await
    }
    async fn unpin(&self, node_id: &str) -> Result<(), crate::StoreError> {
        InMemorySessionStoreFactory::unpin(self, node_id).await
    }
    async fn fork_points(&self) -> Result<Vec<crate::ForkPoint>, crate::StoreError> {
        InMemorySessionStoreFactory::fork_points(self).await
    }
    async fn fork_at(
        &self,
        request: &crate::ForkSessionRequest,
    ) -> Result<crate::ForkSessionReceipt, crate::StoreError> {
        InMemorySessionStoreFactory::fork_at(self, request).await
    }
}

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl crate::store::ConformanceSessionStoreFactory for InMemorySessionStoreFactory {
    async fn create_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<std::sync::Arc<dyn crate::store::ConformancePersistence>, crate::StoreError> {
        Ok(self.create_in_memory_store(request)?)
    }

    async fn open_existing_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<std::sync::Arc<dyn crate::store::ConformancePersistence>>, String> {
        Ok(self
            .open_existing_in_memory_store(request)
            .map(|store| store as std::sync::Arc<dyn crate::store::ConformancePersistence>))
    }
}

#[cfg(any(test, feature = "testing"))]
pub fn in_memory_lineage_handles() -> crate::testing::lineage::LineageConformanceHandles {
    crate::testing::lineage::handles_from_concrete(
        lash_core_memory::in_memory_store::in_memory_lineage_handles(),
    )
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn memory_factory_trait_adapter_uses_same_store() {
        let concrete = crate::InMemorySessionStoreFactory::new();
        let request = crate::testing::store_fixtures::session_store_request(
            &crate::SessionId::from("factory-adapter"),
            "model",
            crate::SessionRelation::Root,
        );
        let direct = concrete.create_store(&request).await.unwrap();
        let facade: std::sync::Arc<dyn crate::SessionStoreFactory> = std::sync::Arc::new(concrete);
        let reopened = facade.open_existing_store(&request).await.unwrap().unwrap();
        assert!(std::sync::Arc::ptr_eq(&direct, &reopened));
        assert!(
            !facade
                .session_was_deleted(&request.session_id)
                .await
                .unwrap()
        );
    }
}
