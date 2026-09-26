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

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`memory_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller. Under
/// `Scheduling::Serial`, a scripted provider that waits on the test holds
/// `double.server().outside_gates().enter()` across the wait.
pub async fn kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate server double")
}

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`memory_backend`] for a test that reaches
/// only store ports.
pub async fn memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    stores
}

/// [`memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
pub async fn memory_store_backend() -> lash_core_execution::Backend {
    lash_conformance::recording_backend_over(memory_store_set().await)
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

/// The ports a hand-built dispatch context runs over: the controller its
/// tool attempts run through and an ephemeral attachment facade. The context
/// takes `effect_controller: ports.controller`, so it cannot outlive a
/// handler that lent the controller.
pub struct DispatchPorts<'h> {
    pub controller: crate::runtime::RuntimeEffectControllerHandle<'h>,
    pub attachment_store: std::sync::Arc<crate::SessionAttachmentStore>,
}

/// The runtime-operation scope a hand-built dispatch context's attempts run
/// under, the one `RuntimeEffectControllerHandle::shared` admits. Open the
/// handler [`double_dispatch_ports`] lends for it.
pub fn dispatch_scope() -> crate::AdmittedScope {
    crate::AdmittedScope::runtime_operation("test-runtime-effect-controller")
}

/// A fresh server double under `seed` and a handler open on it for
/// [`dispatch_scope`]. Build the context over
/// [`double_dispatch_ports`]`(&double, &handler)`, drop it, then close the
/// handler.
pub async fn open_dispatch_handler(
    seed: u64,
) -> (
    lash_restate_test::RestateTestBackend,
    lash_restate_test::OpenHandler,
) {
    let double = kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(dispatch_scope())
        .await
        .expect("open the dispatch handler");
    (double, handler)
}

/// Dispatch ports on the server double: `handler`, opened on `double` for
/// [`dispatch_scope`], lends the controller, and the attachment facade is
/// over the double's attachment port.
pub fn double_dispatch_ports<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
) -> DispatchPorts<'h> {
    DispatchPorts {
        controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(handler.scoped()),
        attachment_store: std::sync::Arc::new(crate::SessionAttachmentStore::ephemeral(
            double.lash_backend().attachment_store(),
        )),
    }
}

/// Dispatch ports for a fixture that brings its own controller (a replaying
/// or recording one): `controller` serves the attempts under
/// [`dispatch_scope`], and the attachment facade is over a storage-only
/// backend. No engine runs.
pub async fn controller_dispatch_ports(
    controller: std::sync::Arc<dyn crate::RuntimeEffectController>,
) -> DispatchPorts<'static> {
    DispatchPorts {
        controller: crate::runtime::RuntimeEffectControllerHandle::shared(controller),
        attachment_store: std::sync::Arc::new(crate::SessionAttachmentStore::ephemeral(
            memory_store_backend().await.attachment_store(),
        )),
    }
}
