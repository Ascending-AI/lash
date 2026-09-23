mod cell_conformance;

use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// Backends opened on this test thread, held until the thread ends: a
    /// memory backend's effect journal reaches its process registry by name,
    /// so the backend must outlive every context built over its ports.
    static HELD_BACKENDS: RefCell<Vec<lash_sqlite_store::SqliteBackend>> =
        const { RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory backend (ADR 0102), held for the rest of the test.
pub(crate) async fn memory_backend() -> lash_sqlite_store::SqliteBackend {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a memory backend");
    HELD_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// The ports of a fresh memory backend: the host a cell's effects journal
/// on, its process-exec-env store and attachment port, and its clock.
pub(crate) async fn memory_backend_ports() -> lash_core::testing::TestExecutionPorts {
    lash_core::testing::TestExecutionPorts::of(&memory_backend().await)
}

/// A fresh memory backend's process registry, for a trigger router whose
/// deliveries no law inspects.
pub(crate) async fn memory_process_registry() -> Arc<dyn lash_core::ProcessRegistry> {
    memory_backend().await.process_registry()
}

/// Ports over a host the test built itself (a capturing or faulting layer),
/// with a fresh memory backend's process-exec-env store beside it.
pub(crate) async fn ports_over_host(
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> lash_core::testing::TestExecutionPorts {
    lash_core::testing::TestExecutionPorts::over_host(
        effect_host,
        memory_backend().await.process_env_store(),
    )
}
