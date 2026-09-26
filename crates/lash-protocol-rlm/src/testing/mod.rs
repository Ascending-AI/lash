mod cell_conformance;
mod kernel_door_tests;

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

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`memory_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller. Under
/// `Scheduling::Serial`, a scripted provider that waits on the test holds
/// `double.server().outside_gates().enter()` across the wait.
pub(crate) async fn kernel_double(
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
pub(crate) async fn memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
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
pub(crate) async fn memory_store_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(memory_store_set().await)
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
pub(crate) async fn memory_artifact_store() -> lashlang::LashlangArtifacts {
    let existing = ARTIFACT_BACKEND.with(|held| held.borrow().clone());
    let backend = match existing {
        Some(backend) => backend,
        None => {
            let backend = memory_backend().await;
            ARTIFACT_BACKEND.with(|held| *held.borrow_mut() = Some(backend.clone()));
            backend
        }
    };
    lashlang::LashlangArtifacts::of_backend(&backend.clone().into())
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
pub(crate) fn memory_artifact_store_blocking() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&memory_backend_blocking().clone().into())
}

/// A fresh memory backend's Lashlang artifact store, isolated from every
/// other law's.
pub(crate) async fn fresh_memory_artifact_store() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&memory_backend().await.into())
}

/// The ports of a fresh SQLite memory backend: the host a cell's effects
/// journal on, its process-exec-env store and attachment port, and its clock.
/// Only the laws of the SQLite engine's own journal (a cold reopen of its
/// effect controller, an injected fault in its journal) run on it; every other
/// cell runs on [`double_ports`].
pub(crate) async fn memory_backend_ports() -> lash_core::testing::TestExecutionPorts<'static> {
    lash_core::testing::TestExecutionPorts::of(&memory_backend().await.into())
}

/// The scope a context built with no parent invocation claims: the builder's
/// default test turn. Open the handler [`double_ports`] lends for it.
pub(crate) fn default_cell_scope() -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("test-session"),
        lash_core::TurnId::from("test-turn"),
    )
}

/// The twin of [`memory_backend_ports`] on the server double: every port of
/// `double`, with the controller `handler` lends serving the context's
/// effects, as a Restate deployment serves a turn's cell from the turn's
/// handler.
///
/// Open `handler` on `double` for the scope the context claims
/// ([`default_cell_scope`], or the scope of the invocation the context
/// installs), drop the context, then close the handler. The borrow keeps the
/// context from outliving the handler.
pub(crate) fn double_ports<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
) -> lash_core::testing::TestExecutionPorts<'h> {
    lash_core::testing::TestExecutionPorts::lent(&double.lash_backend(), handler.scoped())
}

/// [`double_ports`] with `layer` in front of the double's host and of the
/// controller `handler` lends, for a law that observes or perturbs the effect
/// seam.
pub(crate) fn double_ports_over_layer<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> lash_core::testing::TestExecutionPorts<'h> {
    let backend = double.lash_backend();
    let lent =
        lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), Arc::clone(&layer))
            .expect("layer the handler's controller");
    lash_core::testing::TestExecutionPorts {
        effect_host: Arc::new(lash_core::testing::LayeredEffectHost::new(
            backend.effect_host(),
            layer,
        )),
        ..lash_core::testing::TestExecutionPorts::lent(&backend, lent)
    }
}

/// A fresh memory store set's process registry, for a trigger router whose
/// deliveries no law inspects.
pub(crate) async fn memory_process_registry() -> Arc<dyn lash_core::ProcessRegistry> {
    lash_core::StoreSet::process_registry(memory_store_set().await.as_ref())
}

/// A fresh memory store set's trigger store.
pub(crate) async fn memory_trigger_store() -> Arc<dyn lash_core::TriggerStore> {
    lash_core::StoreSet::trigger_store(memory_store_set().await.as_ref())
}
