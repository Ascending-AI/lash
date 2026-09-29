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
    /// The SQLite store sets the running test opened. A memory store set's
    /// databases live while any handle does, and its stores reach sibling
    /// databases by name, so the test holds every store set it opened for as
    /// long as it runs. Each test runs on its own thread.
    static TEST_BACKENDS: std::cell::RefCell<Vec<lash_sqlite_store::SqliteStoreSet>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set behind a recording effect host, held for
/// the running test.
pub(crate) async fn memory_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(std::sync::Arc::new(sqlite_memory_backend().await))
}

/// [`memory_backend`] on `clock`: its storage ports read and wait on `clock`.
pub(crate) async fn memory_backend_with_clock(
    clock: std::sync::Arc<dyn lash_core::Clock>,
) -> lash_core::Backend {
    let backend = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
        .await
        .expect("open a clocked SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    lash_conformance::recording_backend_over(std::sync::Arc::new(backend))
}

/// [`memory_backend`] as its concrete SQLite type.
pub(crate) async fn sqlite_memory_backend() -> lash_sqlite_store::SqliteStoreSet {
    let backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`memory_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller.
pub(crate) async fn kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate server double")
}

pub(crate) async fn settle_pending_session_command(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    result: Result<(), lash_core::SessionError>,
    request: &str,
) {
    use lash_core::testing::TestTurnDrive as _;

    let receipt = match result {
        Ok(()) => return,
        Err(lash_core::SessionError::SessionCommandPending(receipt)) => receipt,
        Err(error) => panic!("session command was refused before drive: {error}"),
    };
    let handler = double
        .open_handler(lash_core::AdmittedScope::queue_drain(
            lash_core::SessionId::from(runtime.session_id()),
            request,
        ))
        .await
        .expect("open session command drive handler");
    runtime
        .drive_next_root(
            request,
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped(),
            ),
        )
        .await
        .expect("engine drives accepted session command");
    handler.close().await.expect("close session command drive");
    assert!(matches!(
        runtime
            .settle_session_command(receipt)
            .await
            .expect("read settled session command"),
        lash_core::runtime::SessionCommandSettlement::Durable(_)
    ));
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

/// `backend`'s session catalog as a runtime store: every session a test
/// admits on it is a [`lash_core::store::SessionStore`] view of this one
/// store. `backend` is one [`memory_backend`] opened.
pub(crate) async fn unbound_store(
    backend: &lash_core::Backend,
) -> std::sync::Arc<dyn lash_core::RuntimeStore> {
    backend.session_store_factory()
}

/// The twin of [`unbound_store`] on the Restate server double: the catalog
/// of the double's engine store set, storage only. It reads through
/// [`lash_restate_test::RestateTestBackend::engine_stores`] — the decorated
/// set — so a `backend_with` layer on its session-store factory applies
/// here too.
pub(crate) async fn double_unbound_store(
    double: &lash_restate_test::RestateTestBackend,
) -> std::sync::Arc<dyn lash_core::RuntimeStore> {
    lash_core::StoreSet::session_store_factory(double.engine_stores().as_ref())
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

/// `unbound_recording_store` on any backend's session catalog — storage only.
/// The store-set twin needs no concrete store type, so it serves
/// [`memory_store_backend`] and a double's `lash_backend` alike: a test that
/// runs no effect still gets the recording decorator's seams.
pub(crate) async fn recording_unbound_store_on(
    backend: &lash_core::Backend,
) -> std::sync::Arc<lash_core::testing::runtime_helpers::RecordingStore> {
    std::sync::Arc::new(lash_core::testing::runtime_helpers::RecordingStore::over(
        backend.session_store_factory(),
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
        clocked.open_store().await.expect("open an unbound store"),
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
    lash_conformance::recording_backend_over(std::sync::Arc::new(reopened))
}

/// The view of `session_id` on `store`, a catalog store the test admitted
/// the session on.
pub(crate) fn session_view(
    store: std::sync::Arc<dyn lash_core::RuntimeStore>,
    session_id: impl Into<lash_core::SessionId>,
) -> lash_core::store::SessionStore {
    lash_core::store::SessionStore::new(store, session_id.into()).expect("a valid session id")
}

/// The current window of `session_id` on `store`: its committed head, which
/// exists.
pub(crate) async fn durable_window(
    store: std::sync::Arc<dyn lash_core::RuntimeStore>,
    session_id: impl Into<lash_core::SessionId>,
) -> lash_core::store::SessionWindowRead {
    session_view(store, session_id)
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await
        .expect("load the session window")
        .expect("the session has a committed head")
}

/// The durable state of `session_id` on `store` at its current window, as a
/// reopen adopts it.
pub(crate) async fn durable_state(
    store: std::sync::Arc<dyn lash_core::RuntimeStore>,
    session_id: impl Into<lash_core::SessionId>,
) -> lash_core::RuntimeSessionState {
    lash_core::store::load_session_window_state(
        &session_view(store, session_id),
        lash_core::store::WindowSelector::Current,
    )
    .await
    .expect("load the durable session state")
    .expect("the session has a committed head")
    .state
}
