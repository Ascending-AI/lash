use super::super::prompt::benchmark_prompt;
use super::*;

#[tokio::test]
async fn rlm_globals_keeps_fixed_session_projection_across_real_turns() {
    super::super::smoke::execute(true, RuntimePerfScenario::RlmGlobals, 1, async {
        let mut runtime =
            build_runtime_with_store(RuntimePerfScenario::RlmGlobals, None, None).await?;
        seed_runtime_state(&mut runtime, RuntimePerfScenario::RlmGlobals).await?;
        let turn = runtime
            .run_turn(
                lash::TurnInput::text(benchmark_prompt(RuntimePerfScenario::RlmGlobals, 1)),
                CancellationToken::new(),
            )
            .await?;
        validate_runtime_perf_turn(RuntimePerfScenario::RlmGlobals, 1, &turn)
    })
    .await
    .expect("RLM globals benchmark should reuse one fixed session projection");
}
