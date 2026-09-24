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

thread_local! {
    /// The memory backend whose artifact store every executor-level law on
    /// this test thread shares, as one host shares one backend.
    static ARTIFACT_BACKEND: RefCell<Option<lash_sqlite_store::SqliteBackend>> =
        const { RefCell::new(None) };
}

/// The Lashlang artifact store of this test thread's memory backend, for a
/// law that reaches artifacts but builds no runtime over a backend. Every
/// call on one thread reaches the same store.
pub(crate) async fn memory_artifact_store() -> Arc<dyn lashlang::LashlangArtifactStore> {
    let existing = ARTIFACT_BACKEND.with(|held| held.borrow().clone());
    let backend = match existing {
        Some(backend) => backend,
        None => {
            let backend = memory_backend().await;
            ARTIFACT_BACKEND.with(|held| *held.borrow_mut() = Some(backend.clone()));
            backend
        }
    };
    lashlang::LashlangArtifactBackend::lashlang_artifact_store(&backend)
}

/// [`memory_backend`] for a synchronous law: the backend opens on a runtime
/// of its own thread, so a caller inside or outside a runtime can use it.
pub(crate) fn memory_backend_blocking() -> lash_sqlite_store::SqliteBackend {
    let backend = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build a current-thread runtime")
                    .block_on(lash_sqlite_store::SqliteBackend::memory())
                    .expect("open a memory backend")
            })
            .join()
            .expect("open the memory backend on its own thread")
    });
    HELD_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// [`fresh_memory_artifact_store`] for a synchronous law.
pub(crate) fn memory_artifact_store_blocking() -> Arc<dyn lashlang::LashlangArtifactStore> {
    lashlang::LashlangArtifactBackend::lashlang_artifact_store(&memory_backend_blocking())
}

/// A fresh memory backend's Lashlang artifact store, isolated from every
/// other law's.
pub(crate) async fn fresh_memory_artifact_store() -> Arc<dyn lashlang::LashlangArtifactStore> {
    lashlang::LashlangArtifactBackend::lashlang_artifact_store(&memory_backend().await)
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
