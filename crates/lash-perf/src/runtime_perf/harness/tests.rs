use super::*;

#[tokio::test]
async fn rlm_globals_executes_a_seed_turn_with_its_retained_session_store() {
    super::super::smoke::execute(true, RuntimePerfScenario::RlmGlobals, 1, async {
        let mut runtime =
            build_runtime_with_store(RuntimePerfScenario::RlmGlobals, None, None).await?;
        seed_runtime_state(&mut runtime, RuntimePerfScenario::RlmGlobals).await
    })
    .await
    .expect("RLM globals benchmark should open and execute its seed turn");
}
