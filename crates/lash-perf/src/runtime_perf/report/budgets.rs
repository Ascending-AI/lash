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

/// Which span of scenario execution carries the guarded allocation and wall-clock ceilings.
///
/// For 38 scenarios this is `WholeRun` (comparing `total_alloc_bytes` and `total_ms`).
/// For `ProcessListStress` this is `RunTurn` (comparing `run_turn_alloc_bytes` and
/// `run_turn_ms`), keeping the test-only quadratic fixture population out of the
/// release guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GuardedSpan {
    WholeRun,
    RunTurn,
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
    guarded_span: GuardedSpan,
    enforced_allocation: EnforcedAllocationBudget,
    advisory_duration: AdvisoryDurationBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnforcedAllocationBudget {
    alloc_bytes_max: f64,
    steady_state_turn_alloc_bytes_max: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvisoryDurationBudget {
    ms_max: f64,
    phases: BTreeMap<String, AdvisoryPhaseBudget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvisoryPhaseBudget {
    budget_ms: f64,
    required: bool,
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

pub(super) fn required_phase_names(scenario: RuntimePerfScenario) -> &'static [&'static str] {
    static REQUIRED_PHASES: OnceLock<BTreeMap<String, Vec<&'static str>>> = OnceLock::new();
    REQUIRED_PHASES
        .get_or_init(|| {
            budgets()
                .scenarios
                .iter()
                .map(|(scenario, budget)| {
                    let required = budget
                        .advisory_duration
                        .phases
                        .iter()
                        .filter_map(|(phase, budget)| budget.required.then_some(phase.as_str()))
                        .collect();
                    (scenario.clone(), required)
                })
                .collect()
        })
        .get(scenario.name())
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("missing runtime budget for {}", scenario.name()))
}

pub(super) fn guarded_span(scenario: RuntimePerfScenario) -> GuardedSpan {
    scenario_budget(scenario).guarded_span
}

pub(super) fn allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .enforced_allocation
        .alloc_bytes_max
}

pub(super) fn steady_state_turn_allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario)
        .enforced_allocation
        .steady_state_turn_alloc_bytes_max
}

pub(super) fn wall_clock_budget_ms(scenario: RuntimePerfScenario) -> f64 {
    scenario_budget(scenario).advisory_duration.ms_max
}

pub(super) fn phase_wall_clock_budget_ms(
    scenario: RuntimePerfScenario,
    phase: &str,
) -> Option<f64> {
    scenario_budget(scenario)
        .advisory_duration
        .phases
        .get(phase)
        .map(|budget| budget.budget_ms)
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
pub(super) fn configured_scenario_names() -> impl Iterator<Item = &'static str> {
    budgets().scenarios.keys().map(String::as_str)
}

#[cfg(test)]
pub(super) fn assert_complete_runtime_budget(scenario: RuntimePerfScenario) {
    let budget = scenario_budget(scenario);
    assert!(budget.enforced_allocation.alloc_bytes_max > 0.0);
    assert!(budget.enforced_allocation.steady_state_turn_alloc_bytes_max > 0.0);
    assert!(budget.advisory_duration.ms_max > 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_list_stress_is_the_only_run_turn_scenario() {
        for scenario in RuntimePerfScenario::KNOWN {
            let expected_span = if scenario == RuntimePerfScenario::ProcessListStress {
                GuardedSpan::RunTurn
            } else {
                GuardedSpan::WholeRun
            };
            assert_eq!(
                guarded_span(scenario),
                expected_span,
                "scenario {} has unexpected guarded span {:?}",
                scenario.name(),
                guarded_span(scenario),
            );
        }
    }

    #[test]
    fn missing_guarded_span_fails_to_load() {
        let json = r#"{
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing field `guarded_span`"),
            "{err}"
        );
    }

    #[test]
    fn invalid_guarded_span_variant_fails_to_load() {
        let json = r#"{
            "guarded_span": "invalid_span",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("unknown variant `invalid_span`"),
            "{err}"
        );
    }

    #[test]
    fn missing_allocation_ceiling_fails_to_load() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing field `alloc_bytes_max`"),
            "{err}"
        );
    }

    #[test]
    fn missing_steady_state_turn_allocation_ceiling_fails_to_load() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing field `steady_state_turn_alloc_bytes_max`"),
            "{err}"
        );
    }

    #[test]
    fn missing_advisory_duration_ceiling_fails_to_load() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(err.to_string().contains("missing field `ms_max`"), "{err}");
    }

    #[test]
    fn missing_phase_budget_fails_to_load_under_deny_unknown_fields() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {
                    "phase": {"required": true}
                }
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing field `budget_ms`"),
            "{err}"
        );
    }

    #[test]
    fn missing_phase_required_flag_fails_to_load_under_deny_unknown_fields() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {
                    "phase": {"budget_ms": 1.0}
                }
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("missing field `required`"),
            "{err}"
        );
    }

    #[test]
    fn unknown_phase_budget_field_fails_to_load_under_deny_unknown_fields() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {
                    "phase": {"budget_ms": 1.0, "required": true, "unexpected": false}
                }
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string().contains("unknown field `unexpected`"),
            "{err}"
        );
    }

    #[test]
    fn legacy_total_alloc_bytes_key_fails_to_load_under_deny_unknown_fields() {
        let json = r#"{
            "guarded_span": "whole_run",
            "enforced_allocation": {
                "total_alloc_bytes_max": 1000.0,
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown field `total_alloc_bytes_max`"),
            "{err}"
        );
    }

    #[test]
    fn legacy_run_turn_keys_fail_to_load_under_deny_unknown_fields() {
        let json = r#"{
            "guarded_span": "run_turn",
            "enforced_allocation": {
                "run_turn_alloc_bytes_max": 1000.0,
                "alloc_bytes_max": 1000.0,
                "steady_state_turn_alloc_bytes_max": 500.0
            },
            "advisory_duration": {
                "ms_max": 10.0,
                "phases": {}
            }
        }"#;
        let err = serde_json::from_str::<RuntimeScenarioBudget>(json).unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown field `run_turn_alloc_bytes_max`"),
            "{err}"
        );
    }
}
