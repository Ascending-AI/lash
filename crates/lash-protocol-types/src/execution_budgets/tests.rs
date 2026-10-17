use std::time::Duration;

use super::*;

const MIN: Duration = Duration::from_secs(60);

fn config() -> ExecutionBudgetsConfig {
    ExecutionBudgetsConfig::recommended()
}

/// Budget validation: the defaults construct, and every refusal fires.
#[test]
fn default_budgets_construct_and_every_refusal_fires() {
    let defaults = ExecutionBudgets::new(config()).expect("the defaults are valid");
    assert_eq!(defaults, ExecutionBudgets::recommended());
    assert_eq!(defaults.model_total(), 10 * MIN);
    assert_eq!(defaults.tool_check_phase(), Duration::from_secs(10));
    assert_eq!(defaults.turn_hook_phase(), MIN);
    assert_eq!(defaults.stop_grace(), Duration::from_secs(2));
    let provider = defaults.provider();
    assert_eq!(
        (
            provider.per_request(),
            provider.response_start(),
            provider.chunk_idle(),
            provider.max_attempts()
        ),
        (5 * MIN, 2 * MIN, 2 * MIN, 4)
    );

    let refused = |config: ExecutionBudgetsConfig| {
        ExecutionBudgets::new(config).expect_err("an invalid budget set is refused")
    };
    for (field, value) in [
        ("stop_grace", Duration::ZERO),
        ("turn_hook_phase", Duration::ZERO),
        ("model_total", Duration::from_micros(999)),
        (
            "tool_check_phase",
            MAX_EXECUTION_BUDGET + Duration::from_millis(1),
        ),
    ] {
        let mut config = config();
        match field {
            "stop_grace" => config.stop_grace = value,
            "model_total" => config.model_total = value,
            "tool_check_phase" => config.tool_check_phase = value,
            _ => config.turn_hook_phase = value,
        }
        assert!(
            matches!(refused(config), ExecutionBudgetsError::OutOfRange { field: found, .. } if found == field),
            "{field} = {value:?}"
        );
    }

    let mut over = config();
    over.model_total = 4 * MIN;
    assert!(matches!(
        refused(over),
        ExecutionBudgetsError::DefaultExceedsCeiling {
            default: "provider.per_request",
            ceiling: "model_total",
            ..
        }
    ));

    let mut overflowing = config();
    overflowing.model_total = MAX_EXECUTION_BUDGET;
    overflowing.stop_grace = Duration::from_secs(1);
    assert!(matches!(
        refused(overflowing),
        ExecutionBudgetsError::SumOverflows {
            left: "model_total",
            right: "stop_grace",
            ..
        }
    ));

    for field in ["tool_check_phase", "turn_hook_phase"] {
        let mut overflowing = config();
        match field {
            "tool_check_phase" => overflowing.tool_check_phase = MAX_EXECUTION_BUDGET,
            _ => overflowing.turn_hook_phase = MAX_EXECUTION_BUDGET,
        }
        assert!(
            matches!(refused(overflowing), ExecutionBudgetsError::SumOverflows { left, right: "stop_grace", .. } if left == field)
        );
    }

    for attempts in [0, MAX_PROVIDER_ATTEMPTS + 1, u32::MAX] {
        assert_eq!(
            ProviderAttemptLimits::new(5 * MIN, 2 * MIN, 2 * MIN, attempts),
            Err(ExecutionBudgetsError::UnboundedRetry {
                value: attempts,
                max: MAX_PROVIDER_ATTEMPTS
            })
        );
    }
    assert!(matches!(
        ProviderAttemptLimits::new(MIN, 2 * MIN, MIN, 4),
        Err(ExecutionBudgetsError::DefaultExceedsCeiling {
            default: "provider.response_start",
            ..
        })
    ));
    assert!(matches!(
        ProviderAttemptLimits::new(Duration::ZERO, MIN, MIN, 4),
        Err(ExecutionBudgetsError::OutOfRange {
            field: "provider.per_request",
            ..
        })
    ));
}

/// L-C2, nested half: a model call inside another stretch is clipped to what
/// remains of it, and an un-nested one gets the whole model total.
#[test]
fn a_nested_model_call_is_clipped_to_the_enclosing_remaining_time() {
    let budgets = ExecutionBudgets::recommended();
    let start = 1_000_000;
    let own = budgets.model_call_limit(start, None);
    assert_eq!(own.remaining(start), 10 * MIN);
    assert_eq!(own.slice(start), 5 * MIN);

    let enclosing = ExecutionLimit::starting_at(start, 5 * MIN, 5 * MIN);
    let now = start + 4 * 60_000;
    let nested = budgets.model_call_limit(now, Some(&enclosing));
    assert_eq!(nested.expires_at, enclosing.expires_at);
    assert_eq!(nested.remaining(now), MIN);
    assert_eq!(nested.slice(now), MIN, "the slice never outlasts the total");
    assert!(nested.is_expired(enclosing.expires_at));
    assert_eq!(nested.remaining(enclosing.expires_at + 1), Duration::ZERO);

    let roomy = ExecutionLimit::starting_at(start, 60 * MIN, 60 * MIN);
    assert_eq!(budgets.model_call_limit(start, Some(&roomy)), own);
}
