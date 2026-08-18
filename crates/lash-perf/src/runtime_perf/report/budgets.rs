use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::RuntimePerfScenario;

const PERF_GUARD_BUDGETS_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/perf_guard_budgets.json"
));

#[derive(Debug, Deserialize)]
struct PerfGuardBudgets {
    runtime: RuntimeBudgets,
}

#[derive(Debug, Deserialize)]
struct RuntimeBudgets {
    scenarios: BTreeMap<String, RuntimeScenarioBudget>,
}

/// A runtime scenario's guard budgets, split by enforcement class.
///
/// The split is the checked-in format, not a naming convention: allocation
/// ceilings are load-independent and gate the release, wall-clock ceilings
/// drift by more than an order of magnitude on a busy shared runner and are
/// therefore advisory. `deny_unknown_fields` on both halves keeps a
/// misfiled ceiling from silently changing its enforcement class.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeScenarioBudget {
    enforced_allocation: EnforcedAllocationBudget,
    advisory_duration: AdvisoryDurationBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnforcedAllocationBudget {
    #[serde(default)]
    total_alloc_bytes_max: Option<f64>,
    #[serde(default)]
    run_turn_alloc_bytes_max: Option<f64>,
    steady_state_turn_alloc_bytes_max: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvisoryDurationBudget {
    #[serde(default)]
    total_ms_max: Option<f64>,
    #[serde(default)]
    run_turn_ms_max: Option<f64>,
    phases: BTreeMap<String, f64>,
}

fn budgets() -> &'static RuntimeBudgets {
    static BUDGETS: OnceLock<RuntimeBudgets> = OnceLock::new();
    BUDGETS.get_or_init(|| {
        serde_json::from_str::<PerfGuardBudgets>(PERF_GUARD_BUDGETS_JSON)
            .expect("scripts/perf_guard_budgets.json must contain valid runtime budgets")
            .runtime
    })
}

fn scenario_budget(scenario: RuntimePerfScenario) -> &'static RuntimeScenarioBudget {
    budgets()
        .scenarios
        .get(scenario.name())
        .unwrap_or_else(|| panic!("missing runtime budget for {}", scenario.name()))
}

pub(super) fn allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .enforced_allocation
        .total_alloc_bytes_max
        .unwrap_or_else(|| panic!("missing total allocation budget for {}", scenario.name()))
}

pub(super) fn steady_state_turn_allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .enforced_allocation
        .steady_state_turn_alloc_bytes_max
}

pub(super) fn wall_clock_budget_ms(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .advisory_duration
        .total_ms_max
        .unwrap_or_else(|| panic!("missing total wall-clock budget for {}", scenario.name()))
}

pub(super) fn process_list_run_allocation_budget_bytes() -> f64 {
    scenario_budget(RuntimePerfScenario::ProcessListStress)
        .enforced_allocation
        .run_turn_alloc_bytes_max
        .expect("missing process-list run allocation budget")
}

pub(super) fn process_list_run_wall_clock_budget_ms() -> f64 {
    scenario_budget(RuntimePerfScenario::ProcessListStress)
        .advisory_duration
        .run_turn_ms_max
        .expect("missing process-list run wall-clock budget")
}

pub(super) fn phase_wall_clock_budget_ms(
    scenario: RuntimePerfScenario,
    phase: &str,
) -> Option<f64> {
    scenario_budget(scenario)
        .advisory_duration
        .phases
        .get(phase)
        .copied()
}

#[cfg(test)]
pub(super) fn configured_phase_names(
    scenario: RuntimePerfScenario,
) -> impl Iterator<Item = &'static str> {
    scenario_budget(scenario)
        .advisory_duration
        .phases
        .keys()
        .map(String::as_str)
}

#[cfg(test)]
pub(super) fn assert_complete_runtime_budget(scenario: RuntimePerfScenario) {
    let budget = scenario_budget(scenario);
    assert!(budget.enforced_allocation.steady_state_turn_alloc_bytes_max > 0.0);
    if scenario == RuntimePerfScenario::ProcessListStress {
        assert!(
            budget
                .enforced_allocation
                .run_turn_alloc_bytes_max
                .is_some()
        );
        assert!(budget.advisory_duration.run_turn_ms_max.is_some());
        assert!(budget.enforced_allocation.total_alloc_bytes_max.is_none());
        assert!(budget.advisory_duration.total_ms_max.is_none());
    } else {
        assert!(budget.enforced_allocation.total_alloc_bytes_max.is_some());
        assert!(budget.advisory_duration.total_ms_max.is_some());
        assert!(
            budget
                .enforced_allocation
                .run_turn_alloc_bytes_max
                .is_none()
        );
        assert!(budget.advisory_duration.run_turn_ms_max.is_none());
    }
}
