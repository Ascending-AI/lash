//! Effect fixtures shared by more than one relocated runtime test binary.
//!
//! `runtime::tests::effect` owned these while every suite compiled into one
//! unit-test target; the suites that reach for them now live in different
//! binaries, so they sit here and each binary re-exports them under the
//! historical `runtime::tests::effect` path.

pub(crate) use crate::runtime::tests::*;

pub(crate) mod commit_pins;
pub(crate) mod effect_controller_doubles;
pub(crate) mod effect_recording_authority;

std::thread_local! {
    /// The SQLite backends the running test opened. A memory backend's
    /// databases live while any handle does, and its stores reach sibling
    /// databases by name, so the test holds every backend it opened for as
    /// long as it runs. Each test runs on its own thread.
    static TEST_BACKENDS: std::cell::RefCell<Vec<lash_sqlite_store::SqliteBackend>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory backend (ADR 0102): every store port and the effect
/// host of one named in-memory substrate, held for the running test.
pub(crate) async fn memory_backend() -> lash_core::Backend {
    std::sync::Arc::new(sqlite_memory_backend().await).into()
}

/// [`memory_backend`] on `clock`: every port of the backend, its effect
/// host's timers included, reads and waits on `clock`.
pub(crate) async fn memory_backend_with_clock(
    clock: std::sync::Arc<dyn lash_core::Clock>,
) -> lash_core::Backend {
    let backend = lash_sqlite_store::SqliteBackend::memory_with_clock(clock)
        .await
        .expect("open a clocked SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    std::sync::Arc::new(backend).into()
}

/// [`memory_backend`] as its concrete SQLite type.
pub(crate) async fn sqlite_memory_backend() -> lash_sqlite_store::SqliteBackend {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
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

/// A fresh, unbound session store on `backend`'s catalog: the first session
/// admitted binds it. `backend` is one [`memory_backend`] opened.
pub(crate) async fn unbound_store(
    backend: &lash_core::Backend,
) -> std::sync::Arc<dyn lash_core::RuntimePersistence> {
    let identity = backend.binding_identity().to_string();
    let sqlite = TEST_BACKENDS
        .with(|held| {
            held.borrow()
                .iter()
                .find(|candidate| candidate.identity() == identity)
                .cloned()
        })
        .expect("an unbound store opens on a memory backend this test opened");
    std::sync::Arc::new(sqlite.open_store().await.expect("open an unbound store"))
}

/// The twin of [`unbound_store`] on the Restate server double: a fresh,
/// unbound session store on the double's engine store set, storage only,
/// whose first admitted session binds it. The open reads through
/// [`lash_restate_test::RestateTestBackend::engine_stores`] — the decorated
/// set — so a `backend_with` layer on its session-store factory applies
/// here too.
pub(crate) async fn double_unbound_store(
    double: &lash_restate_test::RestateTestBackend,
) -> std::sync::Arc<dyn lash_core::RuntimePersistence> {
    lash_core::SessionStoreFactory::open_unbound_store(
        lash_core::StoreSet::session_store_factory(double.engine_stores().as_ref()).as_ref(),
    )
    .await
    .expect("open an unbound store on the double's engine store set")
}

/// [`double_unbound_store`] under a recording decorator: the twin of
/// [`unbound_recording_store`]. A test that stamped its store on its own
/// clock builds the double with `ServerConfig::default().time(TimeMode::Manual)`
/// and moves time with `double.server().advance(..)`: the store stamps on the
/// double's clock.
pub(crate) async fn double_unbound_recording_store(
    double: &lash_restate_test::RestateTestBackend,
) -> std::sync::Arc<lash_core::testing::runtime_helpers::RecordingStore> {
    std::sync::Arc::new(lash_core::testing::runtime_helpers::RecordingStore::over(
        double_unbound_store(double).await,
    ))
}

/// [`unbound_store`] under a recording decorator.
pub(crate) async fn unbound_recording_store(
    backend: &lash_core::Backend,
) -> std::sync::Arc<lash_core::testing::runtime_helpers::RecordingStore> {
    std::sync::Arc::new(lash_core::testing::runtime_helpers::RecordingStore::over(
        unbound_store(backend).await,
    ))
}

/// [`unbound_recording_store`] whose store stamps and expires leases on
/// `clock`, over the same databases as `backend`: a second handle on the
/// backend configured with another clock.
pub(crate) async fn unbound_recording_store_with_clock(
    backend: &lash_core::Backend,
    clock: std::sync::Arc<dyn lash_core::Clock>,
) -> std::sync::Arc<lash_core::testing::runtime_helpers::RecordingStore> {
    let identity = backend.binding_identity().to_string();
    let sqlite = TEST_BACKENDS
        .with(|held| {
            held.borrow()
                .iter()
                .find(|candidate| candidate.identity() == identity)
                .cloned()
        })
        .expect("a clocked store opens on a memory backend this test opened");
    let clocked = sqlite
        .reopen_with_clock(clock)
        .await
        .expect("reopen the memory backend on the test clock");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(clocked.clone()));
    std::sync::Arc::new(lash_core::testing::runtime_helpers::RecordingStore::over(
        std::sync::Arc::new(clocked.open_store().await.expect("open an unbound store")),
    ))
}

/// Fresh handles on `backend`'s databases: what a second process over the
/// same substrate is. `backend` is one [`memory_backend`] opened.
pub(crate) async fn reopened_backend(backend: &lash_core::Backend) -> lash_core::Backend {
    let identity = backend.binding_identity().to_string();
    let sqlite = TEST_BACKENDS
        .with(|held| {
            held.borrow()
                .iter()
                .find(|candidate| candidate.identity() == identity)
                .cloned()
        })
        .expect("a reopen names a memory backend this test opened");
    let reopened = sqlite.reopen().await.expect("reopen the memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(reopened.clone()));
    std::sync::Arc::new(reopened).into()
}
