use super::*;

/// The turn-driving laws' fixture: a fresh backend, its effect host and store set, a
/// native process-work substrate over its process registry, and a runner that
/// scopes each turn on the same host.
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
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry),
    )) as Arc<dyn lash_core_execution::ProcessWorkSubstrate>;
    let turn_runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&effect_host));
    let law_backend = backend.as_stores();
    (
        backend,
        "sqlite-turn-runner",
        effect_host,
        law_backend,
        process_work,
        turn_runner,
        // The SQLite host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    )
}

lash_conformance::turn_runner_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::tool_child_turn_cancel_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::admitted_head_redrive_tests!({
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
});

lash_conformance::drive_admission_tests!({
    let (backend, prefix, effect_host, stores, _process_work, turn_runner, _after_law) =
        sqlite_turn_runner_fixture().await;
    (backend, prefix, effect_host, stores, turn_runner)
});
