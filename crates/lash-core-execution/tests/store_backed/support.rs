//! Shared fixtures for the store-backed tests.

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`sqlite_memory_store_backend`] for a test that
/// reaches only store ports.
pub async fn sqlite_memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    stores
}

/// [`sqlite_memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
pub async fn sqlite_memory_store_backend() -> lash_core_execution::Backend {
    lash_conformance::backend_over(sqlite_memory_store_set().await)
}

/// Storage ports for a process law whose default environment is published.
pub async fn sqlite_memory_process_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet>
{
    let stores = sqlite_memory_store_set().await;
    crate::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    stores
}

/// A registry fixture whose held-engine registrations name stored bytes.
pub async fn sqlite_memory_process_store_backend() -> lash_core_execution::Backend {
    let backend = sqlite_memory_store_backend().await;
    crate::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    backend
}

/// Waits until the wall clock has passed `epoch_ms`, so a cutoff one
/// millisecond past a row's stamp is already in the past for the store.
pub async fn after_millisecond_tick(epoch_ms: u64) {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the wall clock is past the epoch")
            .as_millis() as u64;
        if now > epoch_ms {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

/// The kernel traits whose methods the relocated tests call, in scope under
/// `_` names so a test module imports them with one glob.
pub mod prelude {
    pub use crate::{
        ProcessEventLogTestSupport as _, ProcessExecutionEnvStore as _, ProcessLifecycle as _,
        ProcessObserverRegistry as _, ProcessQuery as _, ProcessRegistrar as _,
        ProcessRetention as _,
    };
}

/// A plugin host over `factories` plus the standard-protocol fake a session
/// needs.
///
/// In-crate, the `cfg(test)` build carried a builtin protocol factory, so a
/// relocated test's `PluginHost::new(factories)` gets the protocol here.
pub fn plugin_host(
    mut factories: Vec<std::sync::Arc<dyn crate::plugin::PluginFactory>>,
) -> crate::PluginHost {
    factories.extend(crate::testing::test_standard_protocol_factories());
    crate::PluginHost::new(
        factories,
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    )
}
