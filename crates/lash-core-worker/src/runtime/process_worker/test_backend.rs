//! The memory backend the worker's tests run over (ADR 0102).

use std::sync::Arc;

use crate::RuntimeHostConfig;

std::thread_local! {
    /// The SQLite backends the running test opened. A memory backend's
    /// databases live while any handle does, and its stores reach sibling
    /// databases by name, so the test holds every backend it opened for as
    /// long as it runs. Each test runs on its own thread.
    static TEST_BACKENDS: std::cell::RefCell<Vec<lash_sqlite_store::SqliteBackend>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory backend (ADR 0102), held for the running test.
pub(super) async fn memory_backend() -> Arc<dyn crate::Backend> {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    Arc::new(backend)
}

/// [`memory_backend`] with its process registry under a fault layer: the
/// layer is the backend's registry, so the worker and the test both read
/// through it.
pub(super) async fn faulted_memory_backend() -> (
    Arc<dyn crate::Backend>,
    Arc<crate::testing::ProcessRegistryFaults>,
) {
    let backend = memory_backend().await;
    let faults = Arc::new(crate::testing::ProcessRegistryFaults::new(
        backend.process_registry(),
    ));
    let layer = Arc::clone(&faults);
    let backend = crate::testing::runtime_helpers::LayeredBackend::over(backend)
        .map_process_registry(|_| layer)
        .into_backend();
    (backend, faults)
}

/// The worker tests' host config over `backend`.
pub(super) fn test_host_config(backend: &Arc<dyn crate::Backend>) -> RuntimeHostConfig {
    RuntimeHostConfig::new(
        Arc::clone(backend),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
}
