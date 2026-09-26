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
pub(super) async fn memory_backend() -> crate::Backend {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    Arc::new(backend).into()
}

/// [`memory_backend`] with its process registry under a fault layer: the
/// layer is the backend's registry, so the worker and the test both read
/// through it.
pub(super) async fn faulted_memory_backend()
-> (crate::Backend, Arc<crate::testing::ProcessRegistryFaults>) {
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

/// The twin of [`faulted_memory_backend`] on the Restate server double: the
/// fault layer wraps the store set's process registry before the engine is
/// built, so the engine's own process services and the test both read
/// through it. (Layering the registry over the built backend instead would
/// miss the engine's services.)
pub(super) async fn faulted_kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> (
    lash_restate_test::RestateTestBackend,
    Arc<crate::testing::ProcessRegistryFaults>,
) {
    let mut installed = None;
    let double = lash_restate_test::backend_with(seed, config, |stores| {
        let faults = Arc::new(crate::testing::ProcessRegistryFaults::new(
            stores.process_registry(),
        ));
        installed = Some(Arc::clone(&faults));
        crate::testing::runtime_helpers::LayeredStores::over(stores)
            .map_process_registry(|_| faults)
            .into_store_set()
    })
    .await
    .expect("build the faulted Restate server double");
    let faults = installed.expect("the double decorated its store set");
    (double, faults)
}

/// The worker tests' host config over `backend`.
pub(super) fn test_host_config(backend: &crate::Backend) -> RuntimeHostConfig {
    RuntimeHostConfig::new(
        backend.clone(),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
}

/// The faulted double's fault layer is the registry the backend hands out
/// and the one the engine's own services were built over.
#[tokio::test]
async fn the_faulted_double_faults_the_engine_and_the_backend_alike() {
    let (double, faults) =
        faulted_kernel_double(0x5_2d40, lash_restate_test::ServerConfig::default()).await;
    faults.set_process_read_error(Some(crate::PluginError::Session(
        "injected read failure".to_string(),
    )));
    let through_backend = double
        .lash_backend()
        .process_registry()
        .get_process(&crate::ProcessId::from("absent"))
        .await
        .expect_err("the backend's registry reads through the fault layer");
    assert!(
        through_backend
            .to_string()
            .contains("injected read failure")
    );
    let through_engine = double
        .restate()
        .store_set()
        .process_registry()
        .get_process(&crate::ProcessId::from("absent"))
        .await
        .expect_err("the engine's store set reads through the fault layer");
    assert!(through_engine.to_string().contains("injected read failure"));
}

/// `engine_stores()` is the decorated set the engine was built over, where
/// `stores()` is the set as it was before `decorate_stores` wrapped it: the
/// faulted double's faults apply through the first and not the second.
#[tokio::test]
async fn the_faulted_doubles_engine_stores_carry_the_faults() {
    let (double, faults) =
        faulted_kernel_double(0x5_2d41, lash_restate_test::ServerConfig::default()).await;
    faults.set_process_read_error(Some(crate::PluginError::Session(
        "injected read failure".to_string(),
    )));
    let decorated = double
        .engine_stores()
        .process_registry()
        .get_process(&crate::ProcessId::from("absent"))
        .await
        .expect_err("the engine's decorated store set reads through the fault layer");
    assert!(decorated.to_string().contains("injected read failure"));
    // `stores()` hands out the concrete registry; `get_process` is its
    // `ProcessQuery` answer.
    use lash_core::ProcessQuery;
    let pre_decoration = double
        .stores()
        .process_registry()
        .get_process(&crate::ProcessId::from("absent"))
        .await
        .expect("the pre-decoration set's registry reads past the fault layer");
    assert!(pre_decoration.is_none());
}
