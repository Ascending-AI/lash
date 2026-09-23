use super::*;

/// The turn-driving laws' fixture: a fresh deployment's effect host and
/// process registry, a native process-work substrate over that registry, and a
/// runner that scopes each turn on the same host.
type SqliteTurnRunnerFixture = (
    TestDeployment,
    &'static str,
    Arc<dyn EffectHost>,
    Arc<dyn ProcessRegistry>,
    Arc<dyn lash_core_execution::ProcessWorkSubstrate>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
    fn(&'static str) -> std::future::Ready<()>,
);

async fn sqlite_turn_runner_fixture() -> SqliteTurnRunnerFixture {
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let effect_host = deployment.effect_host() as Arc<dyn EffectHost>;
    let registry = deployment.process_registry() as Arc<dyn ProcessRegistry>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry),
    )) as Arc<dyn lash_core_execution::ProcessWorkSubstrate>;
    let turn_runner = lash_conformance::HostTurnRunner::shared(Arc::clone(&effect_host));
    (
        deployment,
        "sqlite-turn-runner",
        effect_host,
        registry,
        process_work,
        turn_runner,
        // The SQLite host owns no post-law assertion beyond the shared checks.
        |_law| std::future::ready(()),
    )
}

lash_conformance::turn_runner_tests!({ sqlite_turn_runner_fixture().await });

lash_conformance::tool_child_turn_cancel_tests!({ sqlite_turn_runner_fixture().await });
