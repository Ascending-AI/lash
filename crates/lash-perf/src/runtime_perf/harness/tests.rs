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
