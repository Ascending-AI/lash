//! Runs the backend-agnostic durable effect-group suite against the SQLite
//! tier (FIG-1564).
//!
//! The suite itself lives in `lash-core` and is the same one the in-memory
//! reference host answers, so the two tiers are held to one contract rather
//! than two copies of it. The group laws open and close many hosts over one
//! backend's journal: each host is a fresh reopen of the backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core_execution::{EffectHost, GroupExecutors};
use lash_sqlite_store::{SqliteDatabase, SqliteEffectHost};

use super::{SUBSTRATE, with_lease_timings};
use crate::backend_fixture::{TestBackend, sync_await, system_clock};

/// One host over `backend`'s journal, registered with the suite's executor
/// resolver.
///
/// Registration is what makes the host support groups at all: since FIG-1578 a
/// group carries envelopes, and what runs a child is the resolver its host was
/// built with. `None` builds the unregistered host the suite's first two laws
/// are about — the same database, so "a refused open journals nothing" is asked
/// of the journal the wired hosts read.
fn host(
    backend: &TestBackend,
    executors: Option<Arc<dyn GroupExecutors>>,
) -> Arc<SqliteEffectHost> {
    let backend = backend.clone();
    let host = sync_await(async move { backend.reopen().await.effect_host() });
    if let Some(executors) = executors {
        host.register_group_executors(executors)
            .expect("a freshly opened host has no resolver yet");
    }
    host
}

// The durable SQLite tier answers the effect-group contract the same way the
// in-memory reference host does (FIG-1564).
lash_conformance::effect_group_host_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let hosts = backend.clone();
    (backend, move |executors| {
        host(&hosts, executors) as Arc<dyn EffectHost>
    })
});

// A cancelled child's cancellation is journaled as its terminal, and a host
// that was not running when the close happened reads it back (FIG-1564).
lash_conformance::effect_group_cancelled_child_terminal_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let hosts = backend.clone();
    (backend, move |executors| {
        host(&hosts, executors) as Arc<dyn EffectHost>
    })
});

// Retiring a runtime-operation scope removes its group and child rows in one
// transaction and leaves the fence, while an in-flight operation keeps every
// row (FIG-2500).
lash_conformance::effect_group_runtime_retirement_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let hosts = backend.clone();
    let verify = backend.clone();
    (
        backend,
        move |executors| host(&hosts, executors) as Arc<dyn EffectHost>,
        move |(retired, in_flight): (String, String)| async move {
            let conn = verify.raw(SqliteDatabase::EffectReplay);
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
            _envelope: &lash_core_execution::RuntimeEffectEnvelope,
        ) -> Option<lash_core_execution::RuntimeEffectLocalExecutor<'static>> {
            None
        }
    }

    let backend = TestBackend::open(SUBSTRATE).await;
    let host = host(&backend, None);
    // Before any registration the host does no groups, and says so through the
    // group surface itself rather than through a capability flag (FIG-2266).
    let unwired_view = host
        .scoped(lash_core_execution::AdmittedScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert_eq!(
        open_race_group(&unwired_view)
            .await
            .expect_err("an unwired host cannot open a group")
            .code,
        lash_core_execution::RuntimeErrorCode::EffectGroupUnsupported,
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
            lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape,
            "a loser learns its resolver is not the host's"
        );
    }
    // The winner's registration stands whatever the losers did, and that is now
    // read off the refusal's code: a *wired* host that cannot route this child
    // answers with the routing refusal, never with the unsupported-host one.
    // The two codes are what separates "this backend does no groups" from
    // "this backend does groups but has no runner for this child".
    let wired_view = host
        .scoped(lash_core_execution::AdmittedScope::runtime_operation(
            "registration-race",
        ))
        .expect("a scope binds");
    assert_eq!(
        open_race_group(&wired_view)
            .await
            .expect_err("NoChildRuns routes nothing, so the open is refused")
            .code,
        lash_core_execution::RuntimeErrorCode::RuntimeEffectGroupShape,
        "the winning registration stands: a wired host refuses an unroutable \
         child by shape, not by reporting itself group-less"
    );
}

/// Opens a one-child group through `view`, for the two refusal codes above.
async fn open_race_group(
    view: &lash_core_execution::ScopedEffectController<'_>,
) -> Result<lash_core_execution::EffectGroupHandle, lash_core_execution::RuntimeEffectControllerError>
{
    let scope = view.execution_scope().clone();
    let child = lash_core_execution::RuntimeEffectEnvelope::new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope.clone(), "registration-race:child:0")
                .expect("an admitted scope and replay key"),
            lash_core_execution::RuntimeAttribution::none(),
            "sleep",
        ),
        lash_core_execution::RuntimeEffectCommand::Sleep {
            spec: lash_core_execution::SleepSpec::For { duration_ms: 1 },
        },
    );
    let group = lash_core_execution::RuntimeEffectGroup::try_new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope, "registration-race:group")
                .expect("an admitted scope and replay key"),
            lash_core_execution::RuntimeAttribution::none(),
            "effect-group",
        ),
        "registration-race".to_string(),
        vec![child],
        lash_core_execution::GroupWakePolicy::All,
        lash_core_execution::LoserPolicy::RunToCompletion,
    )?;
    view.controller().open_effect_group(group).await
}

/// The lease window a "crashed" process leaves behind: short enough that a
/// test waits it out, long enough that the claim is observed first.
const CRASH_LEASE_MS: u64 = 900;

/// One host over `backend`'s journal with the drain suite's lease window,
/// so a killed process's claims lapse on a scale a test can wait out.
fn host_with_lease(
    backend: &TestBackend,
    executors: Arc<dyn GroupExecutors>,
) -> Arc<SqliteEffectHost> {
    let backend = backend.clone();
    let host = sync_await(async move {
        let ttl = Duration::from_millis(CRASH_LEASE_MS);
        backend
            .reopen_with(
                with_lease_timings(
                    lash_core_execution::facade_support::LeaseTimings::new(ttl, ttl / 3)
                        .expect("the ttl is at least three renew intervals wide"),
                ),
                system_clock(),
            )
            .await
            .effect_host()
    });
    host.register_group_executors(executors)
        .expect("a freshly opened host has no resolver yet");
    host
}

/// A resolver that answers every child with a runner that enters and then
/// parks forever — process A's half of the crash fixture.
struct ParkingExecutors {
    entered: Arc<AtomicUsize>,
}

impl GroupExecutors for ParkingExecutors {
    fn executor_for(
        &self,
        _envelope: &lash_core_execution::RuntimeEffectEnvelope,
    ) -> Option<lash_core_execution::RuntimeEffectLocalExecutor<'static>> {
        let entered = Arc::clone(&self.entered);
        Some(lash_core_execution::RuntimeEffectLocalExecutor::testing(
            move |_| {
                let entered = Arc::clone(&entered);
                async move {
                    entered.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                    unreachable!("a parked child is never polled to completion")
                }
            },
        ))
    }
}

/// A resolver with exactly one answer: its first `executor_for` returns a
/// runner that records the replay key it executed and settles; every later
/// call answers `None`. `asked` counts resolutions, `executed` counts runs.
#[derive(Default)]
struct OneShotExecutors {
    asked: Arc<std::sync::Mutex<Vec<String>>>,
    executed: Arc<std::sync::Mutex<Vec<String>>>,
}

impl GroupExecutors for OneShotExecutors {
    fn executor_for(
        &self,
        envelope: &lash_core_execution::RuntimeEffectEnvelope,
    ) -> Option<lash_core_execution::RuntimeEffectLocalExecutor<'static>> {
        let mut asked = self.asked.lock().expect("asked");
        asked.push(envelope.invocation.replay_key().to_string());
        if asked.len() != 1 {
            return None;
        }
        let executed = Arc::clone(&self.executed);
        Some(lash_core_execution::RuntimeEffectLocalExecutor::testing(
            move |envelope| {
                let key = envelope.invocation.replay_key().to_string();
                let executed = Arc::clone(&executed);
                async move {
                    executed.lock().expect("executed").push(key);
                    Ok(
                        lash_core_execution::RuntimeEffectOutcome::LanguageRuntimeValue {
                            value: serde_json::json!("settled"),
                        },
                    )
                }
            },
        ))
    }
}

/// A one-child `RunToCompletion` group over `scope`, keyed `key`.
fn one_child_group(
    scope: &lash_core_execution::ExecutionScope,
    key: &str,
) -> lash_core_execution::RuntimeEffectGroup {
    let child = lash_core_execution::RuntimeEffectEnvelope::new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope.clone(), format!("{key}:child:0"))
                .expect("an admitted scope and replay key"),
            lash_core_execution::RuntimeAttribution::none(),
            "settle",
        ),
        lash_core_execution::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "settle".to_string(),
        },
    );
    lash_core_execution::RuntimeEffectGroup::try_new(
        lash_core_execution::RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope.clone(), format!("{key}:group"))
                .expect("an admitted scope and replay key"),
            lash_core_execution::RuntimeAttribution::none(),
            "effect-group",
        ),
        key.to_string(),
        vec![child],
        lash_core_execution::GroupWakePolicy::All,
        lash_core_execution::LoserPolicy::RunToCompletion,
    )
    .expect("a one-child group assembles")
}

/// FIG-3409 finding 8: a reopened group lends the resolve-time runner to a
/// retained child when canonical identity matches, even though the retained
/// membership row's `envelope_json` bytes were reformatted in place.
///
/// Process A opens a one-child group whose executor parks forever, is observed
/// holding the claim, and dies with its Tokio runtime. The retained row is
/// then rewritten as pretty-printed JSON — the same `serde_json::Value`,
/// different bytes. Process B reopens the identical honest group with a
/// one-shot resolver: its resolve-time answer must be the runner the retained
/// child is lent. A rule that compared raw retained bytes instead of the
/// canonical identity would refuse the match, ask the resolver again, get
/// `None`, and dispatch nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_honest_reopen_lends_its_staged_runner_when_the_retained_json_is_formatted_differently()
{
    const KEY: &str = "canonical-reopen";
    const CHILD_KEY: &str = "canonical-reopen:child:0";

    let backend = TestBackend::open(SUBSTRATE).await;

    // Process A: open the group, observe the claim row, die with the runtime.
    let crash_backend = backend.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("process A's runtime");
        runtime.block_on(async move {
            let entered = Arc::new(AtomicUsize::new(0));
            let host = host_with_lease(
                &crash_backend,
                Arc::new(ParkingExecutors {
                    entered: Arc::clone(&entered),
                }),
            );
            let scoped = host
                .scoped(lash_core_execution::AdmittedScope::runtime_operation(KEY))
                .expect("a scope binds");
            let group = one_child_group(scoped.execution_scope(), KEY);
            let _handle = scoped
                .controller()
                .open_effect_group(group)
                .await
                .expect("the group opens");
            let conn = crash_backend.raw(SqliteDatabase::EffectReplay);
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let claimed = conn
                    .query_row(
                        "SELECT COUNT(*) FROM runtime_effect_replay
                         WHERE replay_key = ?1 AND status = 'in_progress'",
                        [CHILD_KEY],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("count the claim row");
                if claimed == 1 && entered.load(Ordering::SeqCst) == 1 {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let mut stmt = conn
                        .prepare("SELECT replay_key, status, group_key FROM runtime_effect_replay")
                        .expect("dump");
                    let rows: Vec<(String, String, Option<String>)> = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                        .expect("dump")
                        .collect::<Result<_, _>>()
                        .expect("dump");
                    panic!(
                        "the parked child's claim row never appeared: entered={}, rows={rows:?}",
                        entered.load(Ordering::SeqCst)
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        // The runtime — and with it the host's tasks and connections — is
        // dropped here: what a killed process leaves behind.
    })
    .join()
    .expect("process A runs its phase before dying");

    // Reformat the retained membership row: same JSON value, different bytes.
    let conn = backend.raw(SqliteDatabase::EffectReplay);
    let retained: String = conn
        .query_row(
            "SELECT envelope_json FROM runtime_effect_group_child WHERE group_key = ?1",
            [KEY],
            |row| row.get(0),
        )
        .expect("the retained membership row");
    let parsed: serde_json::Value =
        serde_json::from_str(&retained).expect("the retained row parses");
    let reformatted = serde_json::to_string_pretty(&parsed).expect("pretty-printed JSON");
    assert_ne!(
        retained, reformatted,
        "pretty-printing must change the retained bytes"
    );
    assert_eq!(
        parsed,
        serde_json::from_str::<serde_json::Value>(&reformatted).expect("the rewrite parses"),
        "the rewrite is the same envelope"
    );
    conn.execute(
        "UPDATE runtime_effect_group_child SET envelope_json = ?1
         WHERE group_key = ?2 AND replay_key = ?3",
        rusqlite::params![reformatted, KEY, CHILD_KEY],
    )
    .expect("rewrite the retained row");

    // Wait out the dead process's lease.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let expires: i64 = conn
            .query_row(
                "SELECT lease_expires_at_ms FROM runtime_effect_replay WHERE replay_key = ?1",
                [CHILD_KEY],
                |row| row.get(0),
            )
            .expect("the claim's lease");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;
        if expires < now {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dead process's lease never lapsed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Process B: a fresh host over the same journal, a one-shot resolver, and
    // the identical honest group.
    let executors = Arc::new(OneShotExecutors::default());
    let host = host_with_lease(&backend, Arc::clone(&executors) as Arc<dyn GroupExecutors>);
    let scoped = host
        .scoped(lash_core_execution::AdmittedScope::runtime_operation(KEY))
        .expect("a scope binds");
    let group = one_child_group(scoped.execution_scope(), KEY);
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the honest reopen opens");

    // The retained child settles through the runner the resolve-time answer
    // staged — lent on canonical identity despite the reformatted row.
    let settlement = tokio::time::timeout(
        Duration::from_secs(30),
        scoped
            .controller()
            .await_next_settlement(&mut handle, lash_core_execution::CancellationToken::new()),
    )
    .await
    .expect("the retained child settles inside the budget")
    .expect("the settlement is not a host error");
    assert_eq!(settlement.position, 0);
    assert!(
        settlement.outcome.is_ok(),
        "the lent runner settled the child: {:?}",
        settlement.outcome
    );
    assert_eq!(
        executors.executed.lock().expect("executed").as_slice(),
        &[CHILD_KEY.to_string()],
        "exactly the retained child's replay key ran"
    );
    assert_eq!(
        executors.asked.lock().expect("asked").len(),
        1,
        "the resolver answered once: a byte-matched reopen would have asked again"
    );
    scoped
        .controller()
        .close_effect_group(handle, lash_core_execution::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");
}

// A quiescence-gated retirement leaves a draining scope's rows alone and
// fences nothing; once the drain settles it removes the rows and leaves the
// fence (FIG-2499 fix round 1).
lash_conformance::effect_group_quiescent_retirement_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let hosts = backend.clone();
    let verify = backend.clone();
    (
        backend,
        move |executors| host(&hosts, executors) as Arc<dyn EffectHost>,
        move |scope_id| async move {
            let conn = verify.raw(SqliteDatabase::EffectReplay);
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
