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

// The durable SQLite tier answers the effect-group contract the same way the
// in-memory reference host does (FIG-1564).
lash_conformance::effect_group_host_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-groups.db");
    (dir, move |executors| {
        Arc::new(host(&path, executors)) as Arc<dyn EffectHost>
    })
});

// A cancelled child's cancellation is journaled as its terminal, and a host
// that was not running when the close happened reads it back (FIG-1564).
lash_conformance::effect_group_cancelled_child_terminal_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cancelled-child-terminal.db");
    (dir, move |executors| {
        Arc::new(host(&path, executors)) as Arc<dyn EffectHost>
    })
});

// Retiring a runtime-operation scope removes its group and child rows in one
// transaction and leaves the fence, while an in-flight operation keeps every
// row (FIG-2500).
lash_conformance::effect_group_runtime_retirement_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runtime-operation-retirement.db");
    let make_path = path.clone();
    let verify_path = path;
    (
        dir,
        move |executors| Arc::new(host(&make_path, executors)) as Arc<dyn EffectHost>,
        move |(retired, in_flight): (String, String)| async move {
            let conn = rusqlite::Connection::open(&verify_path).expect("open the effect journal");
            let count = |sql: &str, scope_id: &str| -> i64 {
                conn.query_row(sql, [scope_id], |row| row.get(0))
                    .expect("count journal rows")
            };
            let groups = "SELECT COUNT(*) FROM runtime_effect_group WHERE scope_id = ?1";
            let children = "SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1";
            let fences = "SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1";
            assert_eq!(
                count(groups, &retired),
                0,
                "retired scope keeps no group row"
            );
            assert_eq!(
                count(children, &retired),
                0,
                "retired scope keeps no child row"
            );
            assert_eq!(count(fences, &retired), 1, "retired scope leaves one fence");
            assert_eq!(
                count(groups, &in_flight),
                1,
                "in-flight scope keeps its group"
            );
            assert_eq!(
                count(children, &in_flight),
                2,
                "in-flight scope keeps its children"
            );
            assert_eq!(
                count(fences, &in_flight),
                0,
                "in-flight scope is not fenced"
            );
        },
    )
});

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
    // Before any registration the host does no groups, and says so through the
    // group surface itself rather than through a capability flag (FIG-2266).
    let unwired_view = host
        .scoped(lash_core::ExecutionScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert_eq!(
        open_race_group(&unwired_view)
            .await
            .expect_err("an unwired host cannot open a group")
            .code,
        lash_core::RuntimeErrorCode::EffectGroupUnsupported,
        "a host with no registered resolver has no runner for any child, so it \
         refuses the whole open rather than journaling a group nothing can run"
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
    // The winner's registration stands whatever the losers did, and that is now
    // read off the refusal's code: a *wired* host that cannot route this child
    // answers with the routing refusal, never with the unsupported-host one.
    // The two codes are what separates "this deployment does no groups" from
    // "this deployment does groups but has no runner for this child".
    let wired_view = host
        .scoped(lash_core::ExecutionScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert_eq!(
        open_race_group(&wired_view)
            .await
            .expect_err("NoChildRuns routes nothing, so the open is refused")
            .code,
        lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
        "the winning registration stands: a wired host refuses an unroutable \
         child by shape, not by reporting itself group-less"
    );
}

/// Opens a one-child group through `view`, for the two refusal codes above.
async fn open_race_group(
    view: &lash_core::ScopedEffectController<'_>,
) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
    let scope = view.execution_scope().clone();
    let child = lash_core::RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), "registration-race:child:0")
                .expect("an admitted scope and replay key"),
            lash_core::RuntimeAttribution::none(),
            "sleep",
        ),
        lash_core::RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 1 },
        },
    );
    let group = lash_core::RuntimeEffectGroup::try_new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope, "registration-race:group")
                .expect("an admitted scope and replay key"),
            lash_core::RuntimeAttribution::none(),
            "effect-group",
        ),
        "registration-race".to_string(),
        vec![child],
        lash_core::GroupWakePolicy::All,
        lash_core::LoserPolicy::RunToCompletion,
    )?;
    view.controller().open_effect_group(group).await
}

// A quiescence-gated retirement leaves a draining scope's rows alone and
// fences nothing; once the drain settles it removes the rows and leaves the
// fence (FIG-2499 fix round 1).
lash_conformance::effect_group_quiescent_retirement_tests!({
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("quiescent-retirement.db");
    let make_path = path.clone();
    let verify_path = path;
    (
        dir,
        move |executors| Arc::new(host(&make_path, executors)) as Arc<dyn EffectHost>,
        move |scope_id| async move {
            let conn = rusqlite::Connection::open(&verify_path).expect("open the effect journal");
            let count = |sql: &str| -> i64 {
                conn.query_row(sql, [&scope_id], |row| row.get(0))
                    .expect("count journal rows")
            };
            assert_eq!(
                count("SELECT COUNT(*) FROM runtime_effect_replay WHERE scope_id = ?1"),
                0
            );
            assert_eq!(
                count("SELECT COUNT(*) FROM runtime_effect_group WHERE scope_id = ?1"),
                0
            );
            // The accepted membership retires with the group rows it keys off:
            // a row that outlived them would name environment bytes the
            // severing below is about to reclaim (ADR 0099 section 3).
            assert_eq!(
                count(
                    "SELECT COUNT(*) FROM runtime_effect_group_child
                     WHERE group_key IN (
                         SELECT group_key FROM runtime_effect_group WHERE scope_id = ?1
                     )"
                ),
                0
            );
            assert_eq!(
                count("SELECT COUNT(*) FROM effect_scope_retirements WHERE scope_id = ?1"),
                1
            );
        },
    )
});
