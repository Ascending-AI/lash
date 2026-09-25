//! Shared fixtures for the store-backed tests.

use lash_sqlite_store::SqliteBackend;

std::thread_local! {
    /// The backends the running test opened. A memory backend's
    /// databases live while any handle does, but its effect journal reaches
    /// the registry database by name, so a fixture that hands out only a
    /// controller or a port would otherwise let the registry vanish under
    /// it. Each test runs on its own thread, so this holds every backend
    /// exactly as long as the test that opened it.
    static TEST_BACKENDS: std::cell::RefCell<Vec<SqliteBackend>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Holds `backend` for the rest of the running test.
pub fn hold_for_test(backend: &SqliteBackend) {
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
}

/// A fresh SQLite memory backend, held for the rest of the running test:
/// every persistence port and the effect host of one named in-memory
/// substrate.
pub async fn memory_backend() -> SqliteBackend {
    let backend = SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    hold_for_test(&backend);
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
        AttachmentStore as _, AwaitEventResolver as _, EffectHost as _, ProcessEventLog as _,
        ProcessEventLogTestSupport as _, ProcessExecutionEnvStore as _, ProcessLeases as _,
        ProcessLifecycle as _, ProcessObserverRegistry as _, ProcessQuery as _,
        ProcessRegistrar as _, ProcessRetention as _, ProcessWakeOutbox as _,
        RuntimeEffectController as _, SessionStoreFactory as _, TestProcessRegistryWriteExt as _,
    };
}

/// The backend host's own controller for `admitted`, as the shared handle
/// a local executor's nested process commands run through.
pub fn scoped_controller(
    backend: &SqliteBackend,
    admitted: crate::AdmittedScope,
) -> std::sync::Arc<dyn crate::RuntimeEffectController> {
    use crate::EffectHost as _;
    backend
        .effect_host()
        .scoped_static(admitted)
        .expect("the backend host admits the scope")
        .expect("the backend host lends a static controller")
        .owned_controller()
        .expect("a static controller is shared")
}

/// A fresh memory backend's own controller for the runtime-operation
/// scope `RuntimeEffectControllerHandle::shared` admits: what the dispatch
/// fixtures run their tool attempts through.
pub async fn runtime_operation_controller() -> std::sync::Arc<dyn crate::RuntimeEffectController> {
    let backend = memory_backend().await;
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

/// The ports a hand-built dispatch context runs over, from one memory
/// backend: its controller for the runtime-operation scope
/// `RuntimeEffectControllerHandle::shared` admits, and an ephemeral facade
/// over its attachment port.
pub struct DispatchPorts {
    pub controller: std::sync::Arc<dyn crate::RuntimeEffectController>,
    pub attachment_store: std::sync::Arc<crate::SessionAttachmentStore>,
}

pub async fn dispatch_ports() -> DispatchPorts {
    let backend = memory_backend().await;
    DispatchPorts {
        controller: scoped_controller(
            &backend,
            crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        ),
        attachment_store: std::sync::Arc::new(crate::SessionAttachmentStore::ephemeral(
            crate::Backend::from(backend.clone()).attachment_store(),
        )),
    }
}
