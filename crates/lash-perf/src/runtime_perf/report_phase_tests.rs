use std::collections::BTreeSet;

use super::budgets::{
    assert_complete_runtime_budget, configured_phase_names, configured_scenario_names,
    phase_wall_clock_budget_ms,
};
use super::guards::required_phases;
use super::{RuntimePerfScenario, ScenarioHarnessKind};
use crate::runtime_perf::measurement::phase_name;

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

#[test]
fn every_required_phase_has_a_checked_in_wall_clock_budget() {
    for scenario in RuntimePerfScenario::KNOWN {
        for phase in required_phases(scenario) {
            assert!(
                phase_wall_clock_budget_ms(scenario, phase).is_some(),
                "{} requires unbudgeted phase {phase}",
                scenario.name()
            );
        }
    }
}

#[test]
fn typed_runtime_phase_inventory_is_required_and_budgeted() {
    let standard = required_phases(RuntimePerfScenario::Standard);
    for phase in lash_core::runtime::RuntimeTurnPhase::ALL {
        let name = phase_name(*phase);
        assert!(
            standard.contains(&name),
            "new typed runtime phase {name} is not required by the standard scenario"
        );
        assert!(
            phase_wall_clock_budget_ms(RuntimePerfScenario::Standard, name).is_some(),
            "new typed runtime phase {name} has no wall-clock budget"
        );
    }
}

#[test]
fn runtime_phase_inventory_is_closed_in_both_directions() {
    let known_scenarios = RuntimePerfScenario::KNOWN
        .iter()
        .map(|scenario| scenario.name())
        .collect::<BTreeSet<_>>();
    let budgeted_scenarios = configured_scenario_names().collect::<BTreeSet<_>>();
    assert_eq!(
        budgeted_scenarios, known_scenarios,
        "every budgeted phase must be owned by a known runtime perf scenario"
    );

    for scenario in RuntimePerfScenario::KNOWN {
        assert_complete_runtime_budget(scenario);
        let required = required_phases(scenario)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let budgeted = configured_phase_names(scenario).collect::<BTreeSet<_>>();
        assert!(
            required.is_subset(&budgeted),
            "{} is missing budgets for required phases: {:?}",
            scenario.name(),
            required.difference(&budgeted).collect::<Vec<_>>()
        );
    }
}

#[test]
fn materially_different_shared_phases_use_scenario_budgets() {
    assert!(
        phase_wall_clock_budget_ms(
            RuntimePerfScenario::RlmObliqueStackMix,
            "rlm_lashlang.execute",
        ) > phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "rlm_lashlang.execute")
    );
    assert!(
        phase_wall_clock_budget_ms(RuntimePerfScenario::RlmLargeToolCatalog, "effect_loop")
            > phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "effect_loop")
    );
}

#[test]
fn runtime_perf_direct_counterparts_link_to_correctness_coverage() {
    for scenario in [
        RuntimePerfScenario::Standard,
        RuntimePerfScenario::StandardToolCalls,
        RuntimePerfScenario::Rlm,
        RuntimePerfScenario::RlmProcessHandles,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion,
        RuntimePerfScenario::RlmSubagentSpawn,
        RuntimePerfScenario::TurnCheckpoint,
        RuntimePerfScenario::QueuedWorkClaimStress,
        RuntimePerfScenario::TurnInputIngressInterrupt,
    ] {
        assert!(
            !scenario.correctness_coverage_ids().is_empty(),
            "{} has a direct correctness counterpart but no coverage link",
            scenario.name()
        );
    }
}

#[test]
fn runtime_perf_runtime_scenario_rationales_explain_lower_layer_ownership() {
    for scenario in [
        RuntimePerfScenario::OpenAiResponsesSseParse,
        RuntimePerfScenario::DirectLlmClient,
        RuntimePerfScenario::ProcessListStress,
        RuntimePerfScenario::ScopedEffectController,
        RuntimePerfScenario::StoreReopen,
        RuntimePerfScenario::SqliteStoreReopen,
        RuntimePerfScenario::TurnCheckpoint,
        RuntimePerfScenario::LiveReplayPressure,
        RuntimePerfScenario::QueuedWorkClaimStress,
        RuntimePerfScenario::TurnInputIngressInterrupt,
        RuntimePerfScenario::StoreHardeningHotPaths,
    ] {
        let metadata = RuntimePerfScenario::METADATA
            .iter()
            .find(|metadata| metadata.scenario == scenario)
            .expect("Runtime Scenario perf metadata");
        assert_eq!(
            metadata.scenario_harness,
            ScenarioHarnessKind::RuntimeScenario
        );
        assert!(
            metadata.harness_rationale.contains("below")
                && metadata.harness_rationale.contains("protocol")
                && metadata.harness_rationale.contains("facade"),
            "{} must explain why its Runtime Scenario classification remains below protocol/facade ownership: {}",
            metadata.name,
            metadata.harness_rationale
        );
    }
}
