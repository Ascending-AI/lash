use super::super::scenarios::ScenarioWiring;
use super::*;

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
