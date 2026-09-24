use super::*;

/// The turn-driving laws' fixture: a fresh backend's effect host and the
/// store set it journals beside, an in-process process-work substrate over
/// that set's registry, and a runner that scopes each turn on the same host.
type SqliteTurnRunnerFixture = (
    TestBackend,
    &'static str,
    Arc<dyn EffectHost>,
    Arc<dyn lash_core_execution::StoreSet>,
    Arc<dyn lash_core_execution::ProcessWorkSubstrate>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
    fn(&'static str) -> std::future::Ready<()>,
);

async fn sqlite_turn_runner_fixture() -> SqliteTurnRunnerFixture {
    let backend = TestBackend::open(SUBSTRATE).await;
    let effect_host = backend.effect_host() as Arc<dyn EffectHost>;
    let stores = Arc::new(backend.stores().clone()) as Arc<dyn lash_core_execution::StoreSet>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        stores.process_registry(),
    )) as Arc<dyn lash_core_execution::ProcessWorkSubstrate>;
    let turn_runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&effect_host));
    (
        backend,
        "sqlite-turn-runner",
        effect_host,
        stores,
        process_work,
        turn_runner,
        // The SQLite host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    )
}

lash_conformance::turn_runner_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::tool_child_turn_cancel_tests!({ sqlite_turn_runner_fixture().await });
