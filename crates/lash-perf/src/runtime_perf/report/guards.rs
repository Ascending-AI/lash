//! Perf-guard budget evaluation: which measurements are compared against a
//! checked-in ceiling, and what a failed comparison is allowed to do.

use serde::Serialize;

use super::budgets::{
    GuardedSpan, allocation_budget_bytes, guarded_span, phase_wall_clock_budget_ms,
    required_phase_names, steady_state_turn_allocation_budget_bytes, wall_clock_budget_ms,
};
use super::{RuntimePerfScenario, RuntimePerfScenarioSummary};

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
    /// Wall-clock ceiling. Advisory only: on shared runners the same binary
    /// measured 7.67 ms then 1.625 ms against a 0.25 ms phase ceiling, and no
    /// duration-keeping remedy survives that (wide margins detect nothing,
    /// median-of-N still misses under sustained load, isolated hardware does
    /// not exist here). A flaky hard gate trains reruns, which is worse than
    /// no gate — so these report and never fail.
    Duration,
}

impl RuntimePerfGuardClass {
    /// Every class ever defined, so the enforcement rules can be proven
    /// exhaustively rather than sampled.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Inventory, Self::Allocation, Self::Duration];

    /// Advisory results are reported but never turn into a non-zero exit.
    /// Written as an exhaustive match on purpose: a new guard class must
    /// declare whether it gates instead of silently inheriting "enforced".
    fn is_advisory(self) -> bool {
        match self {
            Self::Inventory | Self::Allocation => false,
            Self::Duration => true,
        }
    }
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
/// Empty means the enforced classes all held — advisory exceedances live in
/// [`report_advisory_exceedances`] and never reach here.
pub(super) fn enforcement_failures(
    results: &[RuntimePerfBudgetResult],
    inventory_only: bool,
) -> Vec<String> {
    results
        .iter()
        .filter(|result| gates_run(result, inventory_only))
        .map(RuntimePerfBudgetResult::describe)
        .collect()
}

/// Whether a failed guard result is allowed to fail the process. Advisory
/// (wall-clock) results never are; `--enforce-inventory` narrows the rest to
/// the machine-independent inventory class.
pub(super) fn gates_run(result: &RuntimePerfBudgetResult, inventory_only: bool) -> bool {
    if result.passed || result.class.is_advisory() {
        return false;
    }
    !inventory_only || result.class == RuntimePerfGuardClass::Inventory
}

/// Wall-clock exceedances never fail the run, so the only way they stay
/// actionable is being printed every time, with the measured value next to
/// the budget it missed.
pub(super) fn report_advisory_exceedances(results: &[RuntimePerfBudgetResult]) {
    let advisories = results
        .iter()
        .filter(|result| !result.passed && result.class.is_advisory())
        .map(RuntimePerfBudgetResult::describe)
        .collect::<Vec<_>>();
    if advisories.is_empty() {
        return;
    }
    eprintln!(
        "warning: runtime perf advisory wall-clock budgets exceeded (reported, not gating):\n{}",
        advisories.join("\n")
    );
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

        for (phase, metrics) in &summary.phase_summary {
            let Some(budget_ms) = phase_wall_clock_budget_ms(*scenario, phase) else {
                budgets.push(RuntimePerfBudgetResult {
                    scenario: summary.scenario.clone(),
                    scenario_harness: summary.scenario_harness.clone(),
                    metric: format!("phase:{phase}:duration_ms"),
                    statistic: "median".to_string(),
                    // The ceiling itself is advisory, but its presence is
                    // machine-independent: an emitted phase nobody budgeted is
                    // an unreviewed measurement surface, and that still gates.
                    class: RuntimePerfGuardClass::Inventory,
                    actual: Some(metrics.duration_ms.median),
                    budget: None,
                    passed: false,
                    reason: Some("emitted phase has no checked-in wall-clock budget".to_string()),
                });
                continue;
            };
            push_max_budget(
                &mut budgets,
                summary,
                &format!("phase:{phase}:duration_ms"),
                "median",
                RuntimePerfGuardClass::Duration,
                metrics.duration_ms.median,
                budget_ms,
            );
        }

        let (alloc_metric, alloc_actual, duration_metric, duration_actual) =
            match guarded_span(*scenario) {
                GuardedSpan::WholeRun => (
                    "total_alloc_bytes",
                    summary.total_alloc_bytes.median,
                    "total_ms",
                    summary.total_ms.median,
                ),
                GuardedSpan::RunTurn => {
                    // The 1,537-row fixture is populated through TestLocalProcessRegistry,
                    // whose test-only map-clone writes are quadratic. Keep that separately
                    // reported setup span out of the product-facing release guard.
                    (
                        "run_turn_alloc_bytes",
                        summary.run_turn_alloc_bytes.median,
                        "run_turn_ms",
                        summary.run_turn_ms.median,
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
        push_max_budget(
            &mut budgets,
            summary,
            duration_metric,
            "median",
            RuntimePerfGuardClass::Duration,
            duration_actual,
            wall_clock_budget_ms(*scenario),
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
    actual: f64,
    budget: f64,
) {
    let exceeded = actual > budget;
    budgets.push(RuntimePerfBudgetResult {
        scenario: summary.scenario.clone(),
        scenario_harness: summary.scenario_harness.clone(),
        metric: metric.to_string(),
        statistic: statistic.to_string(),
        class,
        actual: Some(actual),
        budget: Some(budget),
        passed: !exceeded,
        reason: exceeded.then(|| {
            if class.is_advisory() {
                "metric exceeded advisory guard budget (reported, not gating)".to_string()
            } else {
                "metric exceeded checked-in guard budget".to_string()
            }
        }),
    });
}

pub(super) fn required_phases(scenario: RuntimePerfScenario) -> &'static [&'static str] {
    required_phase_names(scenario)
}

fn steady_state_turn_alloc_bytes(summary: &RuntimePerfScenarioSummary) -> f64 {
    summary
        .steady_state_turn
        .as_ref()
        .unwrap_or(&summary.last_turn)
        .total_alloc_bytes
        .median
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_perf::report::{stack_profile, summarize, tests::run_result};

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
    fn every_guard_class_declares_its_enforcement_under_both_flag_modes() {
        use RuntimePerfGuardClass::{Allocation, Duration, Inventory};

        for class in RuntimePerfGuardClass::ALL {
            assert!(
                !gates_run(&guard_result(class, true), false),
                "{class:?}: a passing result never fails the run"
            );
            assert!(
                !gates_run(&guard_result(class, true), true),
                "{class:?}: a passing result never fails the inventory run"
            );
        }

        // Inventory checks are machine-independent, so they gate both the
        // PR-time inventory run and the release budget run.
        assert!(gates_run(&guard_result(Inventory, false), true));
        assert!(gates_run(&guard_result(Inventory, false), false));
        // Allocation ceilings are release-calibrated: enforced by
        // --enforce-budgets, skipped by --enforce-inventory.
        assert!(!gates_run(&guard_result(Allocation, false), true));
        assert!(gates_run(&guard_result(Allocation, false), false));
        // Wall-clock ceilings are advisory in every mode (FIG-1385).
        assert!(!gates_run(&guard_result(Duration, false), true));
        assert!(!gates_run(&guard_result(Duration, false), false));
    }

    #[test]
    fn advisory_exceedances_carry_the_measurement_next_to_the_budget() {
        let described = guard_result(RuntimePerfGuardClass::Duration, false).describe();

        assert!(described.contains("actual=7.67"), "{described}");
        assert!(described.contains("budget=0.25"), "{described}");
    }

    #[test]
    fn exceeded_wall_clock_budgets_stay_visible_as_failed_advisory_results() {
        let scenario = RuntimePerfScenario::DeepTurnComposition;
        let results = vec![run_result(scenario, 10.0, 100)];
        let stack_profile = stack_profile(2 * 1024 * 1024);
        let summaries = summarize(&results, &[scenario], 1, &stack_profile);
        let summary = summaries.first().expect("one scenario summary");
        let mut budgets = Vec::new();

        push_max_budget(
            &mut budgets,
            summary,
            "phase:rlm_process.load_artifact:duration_ms",
            "median",
            RuntimePerfGuardClass::Duration,
            7.67,
            0.25,
        );

        let result = budgets.first().expect("one advisory result");
        assert!(!result.passed);
        assert_eq!(result.class, RuntimePerfGuardClass::Duration);
        assert_eq!(
            result.reason.as_deref(),
            Some("metric exceeded advisory guard budget (reported, not gating)")
        );
    }
}
