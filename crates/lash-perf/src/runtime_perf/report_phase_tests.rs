use super::{RuntimePerfScenario, required_phases};

#[test]
fn turn_scenarios_require_the_typed_commit_phase_metrics() {
    for scenario in [
        RuntimePerfScenario::DeepTurnComposition,
        RuntimePerfScenario::RlmAsyncToolCompletion,
        RuntimePerfScenario::RlmTriggerMailPipeline,
        RuntimePerfScenario::RlmLargePrint,
        RuntimePerfScenario::RlmObliqueStackMix,
        RuntimePerfScenario::RlmStreamedPairedLashlang,
        RuntimePerfScenario::RlmProcessHandles,
        RuntimePerfScenario::Standard,
    ] {
        let phases = required_phases(scenario);
        for expected in ["prepared_turn", "committed_turn", "post_commit_delivery"] {
            assert!(
                phases.contains(&expected),
                "{} is missing required phase {expected}",
                scenario.name()
            );
        }
        for removed in [
            "finalize_turn",
            "persist_turn",
            "final_commit",
            "post_persist_hooks",
        ] {
            assert!(
                !phases.contains(&removed),
                "{} still requires removed phase {removed}",
                scenario.name()
            );
        }
    }
}
