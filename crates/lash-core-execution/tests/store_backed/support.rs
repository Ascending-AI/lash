//! Shared fixtures for the store-backed tests.

/// Storage ports over an isolated SQLite memory store set and a recording
/// controller for tests that do not need a durable engine.
pub async fn sqlite_recording_backend() -> lash_core_execution::Backend {
    sqlite_memory_store_backend().await
}

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`sqlite_recording_backend`] for a test that reaches
/// only store ports.
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
    lash_conformance::recording_backend_over(sqlite_memory_store_set().await)
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
        AttachmentStore as _, AwaitEventResolver as _, ProcessEventLog as _,
        ProcessEventLogTestSupport as _, ProcessExecutionEnvStore as _, ProcessLifecycle as _,
        ProcessObserverRegistry as _, ProcessQuery as _, ProcessRegistrar as _,
        ProcessRetention as _, ProcessWakeOutbox as _,
    };
}

/// The backend host's own controller for `admitted`, as the shared handle
/// a local executor's nested process commands run through.
pub fn scoped_controller(
    backend: &lash_core_execution::Backend,
    admitted: crate::AdmittedScope,
) -> std::sync::Arc<dyn crate::RuntimeEffectController> {
    backend
        .effect_host()
        .scoped_static(admitted)
        .expect("the backend host admits the scope")
        .expect("the backend host lends a static controller")
        .owned_controller()
        .expect("a static controller is shared")
}

/// A fresh memory backend's own controller for the runtime-operation
/// scope the dispatch fixtures admit: what the dispatch
/// fixtures run their tool attempts through.
pub async fn runtime_operation_controller() -> std::sync::Arc<dyn crate::RuntimeEffectController> {
    let backend = sqlite_recording_backend().await;
    scoped_controller(
        &backend,
        crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
    )
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
    crate::PluginHost::new(factories)
}

/// The ports a hand-built dispatch context runs over: the controller its
/// tool attempts run through and an ephemeral attachment facade. The context
/// takes `effect_controller: ports.controller`, so it cannot outlive a
/// handler that lent the controller.
pub struct DispatchPorts<'h> {
    pub controller: crate::runtime::ScopedEffectController<'h>,
    pub attachment_store: std::sync::Arc<crate::RuntimeAttachmentStore>,
}

/// Dispatch ports for a fixture that brings its own controller (a replaying
/// or recording one): `controller` serves the attempts under
/// [`dispatch_scope`], and the attachment facade is over a storage-only
/// backend. No engine runs.
pub async fn controller_dispatch_ports(
    controller: std::sync::Arc<dyn crate::RuntimeEffectController>,
) -> DispatchPorts<'static> {
    DispatchPorts {
        controller: crate::runtime::ScopedEffectController::shared(
            controller,
            crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        )
        .expect("valid test runtime scope"),
        attachment_store: std::sync::Arc::new(crate::RuntimeAttachmentStore::ephemeral(
            sqlite_memory_store_backend().await.attachment_store(),
        )),
    }
}
