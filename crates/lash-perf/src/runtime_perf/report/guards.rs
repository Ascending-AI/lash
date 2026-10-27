//! Perf-guard budget evaluation: which measurements are compared against a
//! checked-in ceiling, and what a failed comparison is allowed to do.

use serde::Serialize;

use super::budgets::{
    GuardedSpan, allocation_budget_bytes, guarded_span, phase_is_declared, required_phase_names,
    steady_state_turn_allocation_budget_bytes,
};
use super::{RuntimePerfScenario, RuntimePerfScenarioSummary};
use crate::runtime_perf::measurement::stage;

/// Which guard classes are allowed to fail the process.
///
/// `(true, true)` silently meant "enforce everything", so `--runtime-perf-enforce-inventory`
/// was a no-op whenever `--runtime-perf-enforce-budgets` was also passed, and no help text
/// said so.
/// Three modes, one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BudgetEnforcement {
    #[default]
    None,
    /// Fail only on the machine-independent inventory class. Allocation
    /// ceilings are calibrated on the release profile.
    InventoryOnly,
    /// Fail on inventory and allocation limits.
    All,
}

impl BudgetEnforcement {
    /// The mode the two CLI flags select.
    ///
    /// Passing both is refused rather than silently resolved: it used to mean
    /// "budgets", which is the opposite of what an operator reaching for the
    /// narrower flag is asking for.
    pub fn from_flags(enforce_budgets: bool, enforce_inventory: bool) -> anyhow::Result<Self> {
        match (enforce_budgets, enforce_inventory) {
            (true, true) => anyhow::bail!(
                "--runtime-perf-enforce-inventory is narrower than --runtime-perf-enforce-budgets; pass one"
            ),
            (true, false) => Ok(Self::All),
            (false, true) => Ok(Self::InventoryOnly),
            (false, false) => Ok(Self::None),
        }
    }

    fn enforces(self, class: RuntimePerfGuardClass) -> bool {
        match self {
            Self::None => false,
            Self::InventoryOnly => class == RuntimePerfGuardClass::Inventory,
            Self::All => true,
        }
    }

    /// The label a failure carries, so the operator sees which mode refused.
    pub(super) fn failure_label(self) -> &'static str {
        match self {
            Self::InventoryOnly => "Runtime perf inventory check failed",
            Self::None | Self::All => "Runtime perf budget exceeded",
        }
    }
}

/// What a guard result is allowed to do when it fails.
///
/// The class is a property of the metric, decided once here rather than
/// re-derived from metric-name spelling by every consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePerfGuardClass {
    /// Machine-independent inventory: a required phase must be emitted, and an
    /// emitted phase must carry a checked-in budget. Identical on every
    /// machine, so it gates even the PR-time smoke run.
    Inventory,
    /// Allocation ceiling. Load-independent (within 0.03% across runs on a
    /// runner loaded enough to move wall-clock by 30x), so it gates the
    /// release.
    Allocation,
}

impl RuntimePerfGuardClass {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 2] = [Self::Inventory, Self::Allocation];
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimePerfBudgetResult {
    scenario: String,
    scenario_harness: String,
    metric: String,
    statistic: String,
    class: RuntimePerfGuardClass,
    actual: Option<f64>,
    budget: Option<f64>,
    passed: bool,
    reason: Option<String>,
}

impl RuntimePerfBudgetResult {
    pub(super) fn describe(&self) -> String {
        let actual = self
            .actual
            .map(|value| value.to_string())
            .unwrap_or_else(|| "missing".to_string());
        let budget = self
            .budget
            .map(|value| value.to_string())
            .unwrap_or_else(|| "required".to_string());
        let reason = self.reason.as_deref().unwrap_or("budget exceeded");
        format!(
            "{} {} {} actual={} budget={} ({reason})",
            self.scenario, self.metric, self.statistic, actual, budget
        )
    }
}

/// The failures that must fail the process, already rendered for the error.
/// Empty means the enforced classes all held.
pub(super) fn enforcement_failures(
    results: &[RuntimePerfBudgetResult],
    enforcement: BudgetEnforcement,
) -> Vec<String> {
    results
        .iter()
        .filter(|result| gates_run(result, enforcement))
        .map(RuntimePerfBudgetResult::describe)
        .collect()
}

/// The selected enforcement mode decides which failed counted limits bind.
pub(super) fn gates_run(result: &RuntimePerfBudgetResult, enforcement: BudgetEnforcement) -> bool {
    if result.passed {
        return false;
    }
    enforcement.enforces(result.class)
}

pub(super) fn evaluate_budgets(
    summaries: &[RuntimePerfScenarioSummary],
    scenarios: &[RuntimePerfScenario],
) -> Vec<RuntimePerfBudgetResult> {
    let mut budgets = Vec::new();
    for scenario in scenarios {
        let Some(summary) = summaries
            .iter()
            .find(|summary| summary.scenario == scenario.name())
        else {
            budgets.push(RuntimePerfBudgetResult {
                scenario: scenario.name().to_string(),
                scenario_harness: scenario.scenario_harness().name().to_string(),
                metric: "scenario_output".to_string(),
                statistic: "present".to_string(),
                class: RuntimePerfGuardClass::Inventory,
                actual: None,
                budget: Some(1.0),
                passed: false,
                reason: Some("scenario did not produce a summary".to_string()),
            });
            continue;
        };
        if !scenario.has_guard_budget() {
            continue;
        }

        for phase in required_phases(*scenario) {
            let passed = summary.phase_summary.contains_key(*phase);
            budgets.push(RuntimePerfBudgetResult {
                scenario: scenario.name().to_string(),
                scenario_harness: scenario.scenario_harness().name().to_string(),
                metric: format!("phase:{phase}"),
                statistic: "present".to_string(),
                class: RuntimePerfGuardClass::Inventory,
                actual: if passed { Some(1.0) } else { None },
                budget: Some(1.0),
                passed,
                reason: (!passed).then(|| "required phase metrics were missing".to_string()),
            });
        }

        for phase in summary.phase_summary.keys() {
            if !phase_is_declared(*scenario, phase) {
                budgets.push(RuntimePerfBudgetResult {
                    scenario: summary.scenario.clone(),
                    scenario_harness: summary.scenario_harness.clone(),
                    metric: format!("phase:{phase}"),
                    statistic: "declared".to_owned(),
                    class: RuntimePerfGuardClass::Inventory,
                    actual: None,
                    budget: None,
                    passed: false,
                    reason: Some("emitted phase has no checked-in inventory entry".to_owned()),
                });
            }
        }

        let (alloc_metric, alloc_actual) = match guarded_span(*scenario) {
            GuardedSpan::WholeRun => (
                "total_alloc_bytes",
                stage_alloc_bytes(summary, stage::TOTAL),
            ),
            GuardedSpan::RunTurn => {
                // Populating the 1,537-row registry fixture is setup, reported as its
                // own span. Keep it out of the product-facing release guard.
                (
                    "run_turn_alloc_bytes",
                    stage_alloc_bytes(summary, stage::RUN_TURN),
                )
            }
        };

        push_max_budget(
            &mut budgets,
            summary,
            alloc_metric,
            "median",
            RuntimePerfGuardClass::Allocation,
            alloc_actual,
            allocation_budget_bytes(*scenario),
        );
        push_max_budget(
            &mut budgets,
            summary,
            "steady_state_turn_alloc_bytes",
            "median",
            RuntimePerfGuardClass::Allocation,
            steady_state_turn_alloc_bytes(summary),
            steady_state_turn_allocation_budget_bytes(*scenario),
        );
    }
    budgets
}

pub(super) fn push_max_budget(
    budgets: &mut Vec<RuntimePerfBudgetResult>,
    summary: &RuntimePerfScenarioSummary,
    metric: &str,
    statistic: &str,
    class: RuntimePerfGuardClass,
    actual: Option<f64>,
    budget: f64,
) {
    // A stage the run never reached has no measurement: `actual: None`
    // reports that honestly and passes vacuously, exactly as the old
    // zero-fill did — but without writing a fabricated 0.0 anywhere.
    let exceeded = actual.is_some_and(|actual| actual > budget);
    budgets.push(RuntimePerfBudgetResult {
        scenario: summary.scenario.clone(),
        scenario_harness: summary.scenario_harness.clone(),
        metric: metric.to_string(),
        statistic: statistic.to_string(),
        class,
        actual,
        budget: Some(budget),
        passed: !exceeded,
        reason: exceeded.then(|| "metric exceeded checked-in guard budget".to_string()),
    });
}

pub(super) fn required_phases(scenario: RuntimePerfScenario) -> &'static [&'static str] {
    required_phase_names(scenario)
}

fn steady_state_turn_alloc_bytes(summary: &RuntimePerfScenarioSummary) -> Option<f64> {
    summary
        .steady_state_turn
        .as_ref()
        .unwrap_or(&summary.last_turn)
        .stage_summary
        .get(stage::TOTAL)
        .map(|stage| stage.alloc_bytes.median)
}

fn stage_alloc_bytes(summary: &RuntimePerfScenarioSummary, name: &str) -> Option<f64> {
    summary
        .stage_summary
        .get(name)
        .map(|stage| stage.alloc_bytes.median)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard_result(class: RuntimePerfGuardClass, passed: bool) -> RuntimePerfBudgetResult {
        RuntimePerfBudgetResult {
            scenario: "s".to_string(),
            scenario_harness: "h".to_string(),
            metric: "m".to_string(),
            statistic: "median".to_string(),
            class,
            actual: Some(7.67),
            budget: Some(0.25),
            passed,
            reason: None,
        }
    }

    #[test]
    fn every_guard_class_declares_its_enforcement_in_every_mode() {
        use BudgetEnforcement::{All, InventoryOnly, None};
        use RuntimePerfGuardClass::{Allocation, Inventory};

        // The full table: two classes x three modes x passed/failed. The
        // class axis was already exhaustive; the mode axis used to be two
        // bools sampled at two of their four combinations.
        let expected = [
            (Inventory, None, false),
            (Inventory, InventoryOnly, true),
            (Inventory, All, true),
            (Allocation, None, false),
            // Allocation ceilings are release-calibrated, so the narrow mode
            // skips them and the release mode enforces them.
            (Allocation, InventoryOnly, false),
            (Allocation, All, true),
        ];
        for (class, enforcement, gates) in expected {
            assert_eq!(
                gates_run(&guard_result(class, false), enforcement),
                gates,
                "{class:?} under {enforcement:?}"
            );
            assert!(
                !gates_run(&guard_result(class, true), enforcement),
                "{class:?} under {enforcement:?}: a passing result never fails the run"
            );
        }
        assert_eq!(
            expected.len(),
            RuntimePerfGuardClass::ALL.len() * 3,
            "every class must be listed under every mode"
        );
    }

    #[test]
    fn the_flag_pair_selects_three_modes_and_refuses_the_fourth() {
        assert_eq!(
            BudgetEnforcement::from_flags(false, false).unwrap(),
            BudgetEnforcement::None
        );
        assert_eq!(
            BudgetEnforcement::from_flags(true, false).unwrap(),
            BudgetEnforcement::All
        );
        assert_eq!(
            BudgetEnforcement::from_flags(false, true).unwrap(),
            BudgetEnforcement::InventoryOnly
        );
        // The fourth combination used to resolve silently to "budgets", which
        // made the narrower flag a no-op.
        let both = BudgetEnforcement::from_flags(true, true)
            .unwrap_err()
            .to_string();
        assert!(both.contains("--runtime-perf-enforce-inventory"), "{both}");
        assert!(both.contains("--runtime-perf-enforce-budgets"), "{both}");
    }

    #[test]
    fn durable_scenarios_still_require_a_summary() {
        let scenario = RuntimePerfScenario::DurableCheckpointCurveSqlite;
        let results = evaluate_budgets(&[], &[scenario]);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metric, "scenario_output");
        assert!(!results[0].passed);
        assert_eq!(
            results[0].reason.as_deref(),
            Some("scenario did not produce a summary")
        );
    }
}
