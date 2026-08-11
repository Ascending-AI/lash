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

#[derive(Debug, Deserialize)]
struct RuntimeScenarioBudget {
    #[serde(default)]
    total_alloc_bytes_max: Option<f64>,
    #[serde(default)]
    run_turn_alloc_bytes_max: Option<f64>,
    steady_state_turn_alloc_bytes_max: f64,
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
        .total_alloc_bytes_max
        .unwrap_or_else(|| panic!("missing total allocation budget for {}", scenario.name()))
}

pub(super) fn steady_state_turn_allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario).steady_state_turn_alloc_bytes_max
}

pub(super) fn wall_clock_budget_ms(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .total_ms_max
        .unwrap_or_else(|| panic!("missing total wall-clock budget for {}", scenario.name()))
}

pub(super) fn process_list_run_allocation_budget_bytes() -> f64 {
    scenario_budget(RuntimePerfScenario::ProcessListStress)
        .run_turn_alloc_bytes_max
        .expect("missing process-list run allocation budget")
}

pub(super) fn process_list_run_wall_clock_budget_ms() -> f64 {
    scenario_budget(RuntimePerfScenario::ProcessListStress)
        .run_turn_ms_max
        .expect("missing process-list run wall-clock budget")
}

pub(super) fn phase_wall_clock_budget_ms(
    scenario: RuntimePerfScenario,
    phase: &str,
) -> Option<f64> {
    scenario_budget(scenario).phases.get(phase).copied()
}

#[cfg(test)]
pub(super) fn configured_phase_names(
    scenario: RuntimePerfScenario,
) -> impl Iterator<Item = &'static str> {
    scenario_budget(scenario).phases.keys().map(String::as_str)
}

#[cfg(test)]
pub(super) fn assert_complete_runtime_budget(scenario: RuntimePerfScenario) {
    let budget = scenario_budget(scenario);
    assert!(budget.steady_state_turn_alloc_bytes_max > 0.0);
    if scenario == RuntimePerfScenario::ProcessListStress {
        assert!(budget.run_turn_alloc_bytes_max.is_some());
        assert!(budget.run_turn_ms_max.is_some());
        assert!(budget.total_alloc_bytes_max.is_none());
        assert!(budget.total_ms_max.is_none());
    } else {
        assert!(budget.total_alloc_bytes_max.is_some());
        assert!(budget.total_ms_max.is_some());
        assert!(budget.run_turn_alloc_bytes_max.is_none());
        assert!(budget.run_turn_ms_max.is_none());
    }
}
