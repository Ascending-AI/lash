use super::super::prompt::benchmark_prompt;
use super::super::scenarios::ScenarioWiring;
use super::*;
use tokio_util::sync::CancellationToken;

#[test]
fn rlm_globals_runs_on_the_default_wiring() {
    // Every benchmark core runs its session shift, so the RLM globals lane no
    // longer carves anything out of the default wiring.
    assert_eq!(
        RuntimePerfScenario::RlmGlobals.execution_mode(),
        ExecutionMode::Rlm
    );
    assert_eq!(
        RuntimePerfScenario::RlmGlobals.wiring(),
        ScenarioWiring::DEFAULT
    );
}

#[tokio::test]
async fn rlm_globals_keeps_fixed_session_projection_across_real_turns() {
    super::super::smoke::execute(
        true,
        RuntimePerfScenario::RlmGlobals,
        1,
        Box::pin(async {
            let mut runtime = build_runtime(RuntimePerfScenario::RlmGlobals, None).await?;
            seed_runtime_state(&mut runtime, RuntimePerfScenario::RlmGlobals).await?;
            let turn = runtime
                .run_turn(
                    lash::TurnInput::text(benchmark_prompt(RuntimePerfScenario::RlmGlobals, 1)),
                    CancellationToken::new(),
                )
                .await?;
            validate_runtime_perf_turn(RuntimePerfScenario::RlmGlobals, 1, &turn)
        }),
    )
    .await
    .expect("RLM globals benchmark should reuse one fixed session projection");
}

/// A dropped benchmark runtime frees its in-process lane (FIG-3723): the
/// core, the node it serves and the durable store set under them. Every
/// measured run builds one, so a lane that outlived its run would grow the
/// benchmark's memory by a whole runtime per run. The lane's session store
/// is reachable only through them, so it is freed exactly when they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_runtime_frees_its_in_process_lane() {
    for scenario in [
        RuntimePerfScenario::Standard,
        RuntimePerfScenario::RlmProcessHandles,
    ] {
        let mut runtime = build_runtime(scenario, None)
            .await
            .expect("build the benchmark runtime");
        seed_runtime_state(&mut runtime, scenario)
            .await
            .expect("seed the benchmark session");
        for turn in 1..=2 {
            let report = runtime
                .run_turn(
                    lash::TurnInput::text(benchmark_prompt(scenario, turn)),
                    CancellationToken::new(),
                )
                .await
                .expect("run a benchmark turn");
            validate_runtime_perf_turn(scenario, turn, &report).expect("a valid benchmark turn");
        }
        let lane = Arc::downgrade(&runtime.store());
        drop(runtime);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while lane.strong_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            lane.strong_count(),
            0,
            "{}: the dropped runtime's lane is still held",
            scenario.name()
        );
    }
}
