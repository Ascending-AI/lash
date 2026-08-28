//! Runs the backend-agnostic durable effect-group suite against the SQLite
//! tier (FIG-1564).
//!
//! The suite itself lives in `lash-core` and is the same one the in-memory
//! reference host answers, so the two tiers are held to one contract rather
//! than two copies of it. It sits in its own integration test rather than
//! alongside the process-registry conformance run because the group laws open
//! and close many hosts over one database file, which is a different fixture
//! shape — and because the shared file has a line budget the laws would push
//! past.

use std::future::Future;
use std::sync::Arc;

use lash_core::{EffectHost, GroupExecutors};
use lash_sqlite_store::SqliteEffectHost;

/// Blocks on `future` from a synchronous context.
///
/// The suite's host factory is synchronous by design — a host is a value, not
/// an await — so opening a store-backed one needs a runtime of its own.
fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

/// One host over `path`, registered with the suite's executor resolver.
///
/// Registration is what makes the host support groups at all: since FIG-1578 a
/// group carries envelopes, and what runs a child is the resolver its host was
/// built with. `None` builds the unregistered host the suite's first two laws
/// are about — the same database, so "a refused open journals nothing" is asked
/// of the journal the wired hosts read.
fn host(path: &std::path::Path, executors: Option<Arc<dyn GroupExecutors>>) -> SqliteEffectHost {
    let path = path.to_path_buf();
    let host = sync_await(async move {
        SqliteEffectHost::open(&path)
            .await
            .expect("SQLite effect-group host")
    });
    if let Some(executors) = executors {
        host.register_group_executors(executors)
            .expect("a freshly opened host has no resolver yet");
    }
    host
}

/// The durable SQLite tier answers the effect-group contract the same way the
/// in-memory reference host does (FIG-1564).
///
/// One host object per law, all over one database file: the group surface is a
/// property of the store, not of a particular controller instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_effect_host_satisfies_the_effect_group_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-groups.db");
    lash_core::testing::conformance::effect_group_host_conformance(|executors| {
        Arc::new(host(&path, executors)) as Arc<dyn EffectHost>
    })
    .await;
}

/// A cancelled child's cancellation is journaled as its terminal, and a host
/// that was not running when the close happened reads it back (FIG-1564).
///
/// The reading host is the point: it holds none of the closing host's memory,
/// so the terminal it serves came out of the effect journal or from nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_journals_a_cancelled_child_as_its_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cancelled-child-terminal.db");
    lash_core::testing::conformance::effect_group_cancelled_child_terminal_is_durable(
        |executors| Arc::new(host(&path, executors)) as Arc<dyn EffectHost>,
    )
    .await;
}

/// Two threads registering different resolvers on one host: exactly one wins,
/// and the losers are refused rather than silently dropped (FIG-1578).
///
/// The durable driver's registration site, where the same `OnceLock` race lives
/// as on the native substrate. A `get`-then-`set` pair would hand a loser `Ok` while
/// its resolver went nowhere, so a host would answer a journaled child's routing
/// question through a resolver its wiring code did not think was registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_registration_of_different_resolvers_refuses_every_loser() {
    const REGISTRARS: usize = 8;

    /// A resolver that routes nothing: this law is about which registration
    /// wins, not about what a child does.
    struct NoChildRuns;

    impl GroupExecutors for NoChildRuns {
        fn executor_for(
            &self,
            _envelope: &lash_core::RuntimeEffectEnvelope,
        ) -> Option<lash_core::RuntimeEffectLocalExecutor<'static>> {
            None
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("registration-race.db");
    let host = Arc::new(host(&path, None));
    let unwired_view = host
        .scoped(lash_core::ExecutionScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert!(
        !unwired_view.controller().supports_effect_groups(),
        "a freshly opened host has no resolver, so it does not support groups"
    );
    drop(unwired_view);

    let barrier = Arc::new(std::sync::Barrier::new(REGISTRARS));
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..REGISTRARS)
            .map(|_| {
                let host = Arc::clone(&host);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    // A distinct allocation per thread, so the same-resolver
                    // no-op cannot be mistaken for a winner.
                    let executors = Arc::new(NoChildRuns) as Arc<dyn GroupExecutors>;
                    barrier.wait();
                    host.register_group_executors(executors)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a registrar thread"))
            .collect()
    });

    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "exactly one of {REGISTRARS} different resolvers may be this host's \
         answer to what runs a journaled child"
    );
    for refusal in outcomes.into_iter().filter_map(Result::err) {
        assert_eq!(
            refusal.code,
            lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
            "a loser learns its resolver is not the host's"
        );
    }
    let wired_view = host
        .scoped(lash_core::ExecutionScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert!(
        wired_view.controller().supports_effect_groups(),
        "the winner's registration stands whatever the losers did"
    );
}
