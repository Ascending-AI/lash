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
pub(crate) async fn sqlite_recording_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(std::sync::Arc::new(sqlite_memory_backend().await))
}

/// [`sqlite_recording_backend`] on `clock`: its storage ports read and wait on `clock`.
pub(crate) async fn sqlite_recording_backend_with_clock(
    clock: std::sync::Arc<dyn lash_core::Clock>,
) -> lash_core::Backend {
    let backend = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
        .await
        .expect("open a clocked SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    lash_conformance::recording_backend_over(std::sync::Arc::new(backend))
}

/// [`sqlite_recording_backend`] as its concrete SQLite type.
pub(crate) async fn sqlite_memory_backend() -> lash_sqlite_store::SqliteStoreSet {
    let backend = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a SQLite memory backend");
    TEST_BACKENDS.with(|held| held.borrow_mut().push(backend.clone()));
    backend
}

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`sqlite_recording_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller.
/// The runtime owner's usage once every run it admitted is resolved: the
/// engine delivers each spending effect's settlement after the effect is
/// journaled, asynchronously to the turn (ADR 0125).
pub(crate) async fn settled_runtime_usage(
    runtime: &lash_core::runtime::LashRuntime,
) -> lash_core::OwnerUsage {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let usage = runtime.usage().await.expect("read the owner's usage");
        if usage.completeness.is_settled() || std::time::Instant::now() >= deadline {
            return usage;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

pub(crate) async fn kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate server double")
}

/// Apply `transaction` to a store-backed `runtime`'s config as the shift does
/// (FIG-4379): submit it under `request`, written against the runtime's
/// current config revision, run the runtime's own next shift on `double`,
/// which applies it once no run owns the head, and answer how it settled.
pub(crate) async fn apply_config(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    transaction: lash_core::ConfigTransaction,
    request: &str,
) -> lash_core::ConfigTransactionOutcome {
    let revision = runtime.config_revision();
    let receipt = runtime
        .submit_config_transaction(request, revision, &transaction)
        .await
        .expect("submit the config transaction");
    match Box::pin(execute_submitted_command(runtime, double, receipt, request)).await {
        lash_core::runtime::SessionCommandOutcome::ConfigTransaction { outcome } => outcome,
        other => panic!("a config transaction settles with its own outcome: {other:?}"),
    }
}

/// [`apply_config`], which must apply the transaction.
pub(crate) async fn configure(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    transaction: lash_core::ConfigTransaction,
    request: &str,
) {
    let outcome = Box::pin(apply_config(runtime, double, transaction, request)).await;
    assert!(
        matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
        "the config transaction must apply: {outcome:?}"
    );
}

/// Apply `transaction` to a storeless `runtime`'s config, written against
/// its current config revision, and answer how it settled.
pub(crate) async fn apply_storeless_config(
    runtime: &mut lash_core::runtime::LashRuntime,
    transaction: lash_core::ConfigTransaction,
) -> lash_core::ConfigTransactionOutcome {
    let revision = runtime.config_revision();
    runtime
        .apply_storeless_config_transaction(
            format!("config:{}", uuid::Uuid::new_v4()),
            revision,
            &transaction,
        )
        .await
        .expect("a storeless runtime applies the config transaction")
}

/// [`apply_storeless_config`], which must apply the transaction.
pub(crate) async fn configure_storeless(
    runtime: &mut lash_core::runtime::LashRuntime,
    transaction: lash_core::ConfigTransaction,
) {
    let outcome = Box::pin(apply_storeless_config(runtime, transaction)).await;
    assert!(
        matches!(outcome, lash_core::ConfigTransactionOutcome::Applied { .. }),
        "the config transaction must apply: {outcome:?}"
    );
}

/// Apply a host head write as the session's shift does (FIG-4202): submit
/// `command` to `runtime`'s command lane, run the runtime's own next shift
/// on `double`, which applies it at the turn boundary, and answer the typed
/// outcome it settled with.
pub(crate) async fn apply_host_command(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    command: lash_core::runtime::SessionCommand,
    request: &str,
) -> lash_core::runtime::SessionCommandOutcome {
    use lash_core::testing::TestTurnExecution as _;

    let receipt = runtime
        .submit_session_command(command, request)
        .await
        .expect("submit the host command");
    Box::pin(execute_submitted_command(runtime, double, receipt, request)).await
}

/// Run `runtime`'s own next shift on `double`, which applies the command
/// `receipt` names at the turn boundary, and answer its typed outcome.
async fn execute_submitted_command(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    receipt: lash_core::runtime::SessionCommandReceipt,
    request: &str,
) -> lash_core::runtime::SessionCommandOutcome {
    use lash_core::testing::TestTurnExecution as _;

    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from(runtime.session_id()),
            lash_core::TurnId::fixture(request),
        ))
        .await
        .expect("open the host command's shift handler");
    runtime
        .execute_next_run(
            request,
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                handler.scoped(),
            ),
        )
        .await
        .expect("the shift applies the host command");
    handler
        .close()
        .await
        .expect("close the host command's shift handler");
    match runtime
        .settle_session_command(receipt)
        .await
        .expect("read the host command's settlement")
    {
        lash_core::runtime::SessionCommandSettlement::Applied { outcome, .. } => outcome,
        other => panic!("the host command settles applied: {other:?}"),
    }
}

/// [`apply_host_command`] for an append: the append's typed outcome.
pub(crate) async fn apply_host_append(
    runtime: &mut lash_core::runtime::LashRuntime,
    double: &lash_restate_test::RestateTestBackend,
    request: lash_core::AppendSessionNodesRequest,
) -> lash_core::AppendSessionNodesOutcome {
    let key = request.operation_id.clone();
    match Box::pin(apply_host_command(
        runtime,
        double,
        lash_core::runtime::SessionCommand::AppendSessionNodes {
            request: Box::new(request),
        },
        &key,
    ))
    .await
    {
        lash_core::runtime::SessionCommandOutcome::AppendSessionNodes { outcome } => outcome,
        other => panic!("an append settles with its own outcome: {other:?}"),
    }
}

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`sqlite_recording_backend`] for a test that reaches
/// only store ports.
pub(crate) async fn sqlite_memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
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
pub(crate) async fn sqlite_memory_store_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(sqlite_memory_store_set().await)
}

/// `backend`'s session catalog as a runtime store: every session a test
/// admits on it is a [`lash_core::store::SessionStore`] view of this one
/// store. `backend` is one [`sqlite_recording_backend`] opened.
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
/// [`sqlite_memory_store_backend`] and a double's `lash_backend` alike: a test that
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
/// same substrate is. `backend` is one [`sqlite_recording_backend`] opened.
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
