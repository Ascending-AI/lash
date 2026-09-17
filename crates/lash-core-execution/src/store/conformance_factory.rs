use lash_core_store::store::{ConformancePersistence, StoreError};

/// A [`SessionStoreFactory`](crate::SessionStoreFactory) that can also hand
/// out [`ConformancePersistence`] handles.
///
/// The production factory returns `Arc<dyn RuntimePersistence>`, which has no
/// test hooks; the factory-driven conformance suites take this gated trait and
/// create the handles they probe through it. Backends implement it under their
/// own `testing` gate, typically by sharing the concrete constructor behind
/// their production `create_store`.
#[async_trait::async_trait]
pub trait ConformanceSessionStoreFactory: crate::SessionStoreFactory {
    /// Create a session store exactly as `create_store` would, keeping the
    /// test-support hooks reachable on the returned handle.
    async fn create_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<std::sync::Arc<dyn ConformancePersistence>, StoreError>;

    /// Reopen a session store exactly as `open_existing_store` would, keeping
    /// the test-support hooks reachable on the returned handle.
    async fn open_existing_conformance_store(
        &self,
        request: &crate::SessionStoreCreateRequest,
    ) -> Result<Option<std::sync::Arc<dyn ConformancePersistence>>, String>;
}
