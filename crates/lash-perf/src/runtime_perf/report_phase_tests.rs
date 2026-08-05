use std::collections::BTreeSet;

use super::budgets::{GLOBAL_PHASE_BUDGETS, SCENARIO_KEYED_PHASES};
use super::{
    RuntimePerfScenario, ScenarioHarnessKind, phase_wall_clock_budget_ms, required_phases,
};
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
fn budget_and_required_phase_inventories_are_closed() {
    let required = RuntimePerfScenario::KNOWN
        .into_iter()
        .flat_map(required_phases)
        .copied()
        .collect::<BTreeSet<_>>();
    let budgeted = GLOBAL_PHASE_BUDGETS
        .iter()
        .map(|budget| budget.phase)
        .chain(SCENARIO_KEYED_PHASES.iter().copied())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        budgeted, required,
        "budget names and the union of scenario-required emitted phases drifted"
    );
    assert_eq!(
        GLOBAL_PHASE_BUDGETS.len(),
        GLOBAL_PHASE_BUDGETS
            .iter()
            .map(|budget| budget.phase)
            .collect::<BTreeSet<_>>()
            .len(),
        "global phase budget names must be unique"
    );
    assert!(
        SCENARIO_KEYED_PHASES
            .iter()
            .all(|phase| !GLOBAL_PHASE_BUDGETS
                .iter()
                .any(|budget| budget.phase == *phase)),
        "scenario-keyed phases must not retain a loose global fallback"
    );
}

#[test]
fn materially_different_shared_phases_use_scenario_budgets() {
    assert_eq!(
        phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "rlm_lashlang.execute"),
        Some(5.0)
    );
    assert_eq!(
        phase_wall_clock_budget_ms(
            RuntimePerfScenario::RlmObliqueStackMix,
            "rlm_lashlang.execute",
        ),
        Some(1_200.0)
    );
    assert_eq!(
        phase_wall_clock_budget_ms(RuntimePerfScenario::Rlm, "effect_loop"),
        Some(125.0)
    );
    assert_eq!(
        phase_wall_clock_budget_ms(RuntimePerfScenario::RlmLargeToolCatalog, "effect_loop"),
        Some(6_000.0)
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

#[test]
fn inventory_discriminator_covers_all_four_result_shapes() {
    let result = |statistic: &str, budget: Option<f64>| super::RuntimePerfBudgetResult {
        scenario: "s".to_string(),
        scenario_harness: "h".to_string(),
        metric: "m".to_string(),
        statistic: statistic.to_string(),
        actual: None,
        budget,
        passed: false,
        reason: None,
    };
    // Missing scenario summary and missing required phase are presence checks
    // with a concrete Some(1.0) budget: inventory.
    assert!(super::is_inventory_result(&result("present", Some(1.0))));
    // An emitted phase without a checked-in budget: inventory.
    assert!(super::is_inventory_result(&result("median", None)));
    // Calibrated ceiling comparisons (push_max_budget always passes a literal
    // "median"/"max" statistic and a concrete ceiling): skipped by
    // inventory-only enforcement.
    assert!(!super::is_inventory_result(&result("median", Some(30.0))));
    assert!(!super::is_inventory_result(&result("max", Some(1_500.0))));
}
