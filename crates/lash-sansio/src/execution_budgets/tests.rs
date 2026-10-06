use std::time::Duration;

use super::*;
use crate::{ExpectedExecution, ToolDefinition};

const MIN: Duration = Duration::from_secs(60);

fn config() -> ExecutionBudgetsConfig {
    ExecutionBudgetsConfig::default()
}

/// Budget validation: the defaults construct, and every refusal fires.
#[test]
fn default_budgets_construct_and_every_refusal_fires() {
    let defaults = ExecutionBudgets::new(config()).expect("the defaults are valid");
    assert_eq!(defaults, ExecutionBudgets::default());
    assert_eq!(defaults.tool_default(), 2 * MIN);
    assert_eq!(defaults.tool_ceiling(), 5 * MIN);
    assert_eq!(defaults.model_total(), 10 * MIN);
    assert_eq!(defaults.control_phase(), MIN);
    assert_eq!(defaults.stop_grace(), Duration::from_secs(2));
    assert_eq!(defaults.wait_default(), 60 * MIN);
    assert_eq!(defaults.wait_ceiling(), 24 * 60 * MIN);
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
        ("tool_default", Duration::ZERO),
        ("model_total", Duration::from_micros(999)),
        (
            "control_phase",
            MAX_EXECUTION_BUDGET + Duration::from_millis(1),
        ),
        ("wait_ceiling", Duration::MAX),
    ] {
        let mut config = config();
        match field {
            "tool_default" => config.tool_default = value,
            "model_total" => config.model_total = value,
            "control_phase" => config.control_phase = value,
            _ => config.wait_ceiling = value,
        }
        assert!(
            matches!(refused(config), ExecutionBudgetsError::OutOfRange { field: found, .. } if found == field),
            "{field} = {value:?}"
        );
    }

    let mut over = config();
    over.tool_default = 6 * MIN;
    assert!(matches!(
        refused(over),
        ExecutionBudgetsError::DefaultExceedsCeiling {
            default: "tool_default",
            ..
        }
    ));
    let mut over = config();
    over.wait_default = 25 * 60 * MIN;
    assert!(matches!(
        refused(over),
        ExecutionBudgetsError::DefaultExceedsCeiling {
            default: "wait_default",
            ..
        }
    ));
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
    let mut overflowing = config();
    overflowing.wait_default = MAX_EXECUTION_BUDGET;
    overflowing.wait_ceiling = MAX_EXECUTION_BUDGET;
    assert!(matches!(
        refused(overflowing),
        ExecutionBudgetsError::SumOverflows {
            left: "wait_ceiling",
            right: "tool_ceiling",
            ..
        }
    ));

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
    let budgets = ExecutionBudgets::default();
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

/// L-C3: one admission phase is bounded by one `control_phase`, however
/// many checks it runs; no check gets a bound of its own.
#[test]
fn one_control_phase_bound_covers_every_check_in_the_phase() {
    let budgets = ExecutionBudgets::default();
    let start = 5_000;
    let phase = budgets.control_phase_limit(start);
    let check_cost_ms = 7_000;
    let mut now = start;
    let mut checks_run = 0;
    while !phase.is_expired(now) {
        assert_eq!(
            phase.remaining(now),
            MIN - Duration::from_millis(now - start),
            "check {checks_run} reads the phase's one bound"
        );
        now += check_cost_ms;
        checks_run += 1;
    }
    assert_eq!(
        checks_run, 9,
        "60 s of 7 s checks: the ninth ends past the phase"
    );
    assert!(
        Duration::from_millis(check_cost_ms) < budgets.control_phase(),
        "every check alone fits a per-check bound the phase does not grant"
    );
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "a tool",
        serde_json::json!({"type": "object"}),
        serde_json::json!({"type": "string"}),
    )
    .expect("valid tool schemas")
}

/// L-E: an inline tool declared above the ceiling is refused; isolated,
/// process-starting and Pending tools are accepted with an inline prefix
/// bounded by the ceiling; a tool that declares nothing runs the default.
#[test]
fn the_inline_ceiling_refuses_long_inline_tools_and_admits_long_work() {
    let budgets = ExecutionBudgets::default();
    let forty_minutes = ExpectedExecution::declared(40 * MIN);

    assert_eq!(budgets.admit_tool(&tool("quick").manifest()), Ok(2 * MIN));
    assert_eq!(
        budgets.admit_tool(
            &tool("bounded")
                .with_expected_execution(ExpectedExecution::declared(5 * MIN))
                .manifest()
        ),
        Ok(5 * MIN)
    );
    assert_eq!(
        budgets.admit_tool(
            &tool("run_full_suite")
                .with_expected_execution(forty_minutes)
                .manifest()
        ),
        Err(RegistrationRefused::InlineBudgetExceedsCeiling {
            tool: "run_full_suite".to_string(),
            declared: 40 * MIN,
            ceiling: 5 * MIN,
            hint: INLINE_CEILING_HINT.to_string(),
        })
    );

    for declaration in [
        crate::ToolDeclaration {
            isolated: true,
            ..crate::ToolDeclaration::default()
        },
        crate::ToolDeclaration::default().with_intents([crate::ToolIntentKind::StartProcess]),
        crate::ToolDeclaration::deferring(),
    ] {
        assert_eq!(
            budgets.admit_tool(
                &tool("long_work")
                    .with_expected_execution(forty_minutes)
                    .with_declaration(declaration.clone())
                    .manifest()
            ),
            Ok(5 * MIN),
            "{declaration:?} runs its long work outside the inline prefix"
        );
    }
}

/// The ceiling refuses at registration: a catalog with an inline member
/// declared above it is not built.
#[test]
fn a_catalog_with_an_inline_tool_above_the_ceiling_is_refused() {
    let long =
        tool("run_full_suite").with_expected_execution(ExpectedExecution::declared(40 * MIN));
    let contracts = std::collections::BTreeMap::from([(
        long.manifest().id.clone(),
        std::sync::Arc::new(long.contract()),
    )]);
    let Err(error) = crate::ToolCatalog::from_tools(vec![long.manifest()], contracts) else {
        panic!("the catalog is refused");
    };
    assert!(matches!(
        error,
        crate::ToolCatalogBuildError::RegistrationRefused {
            refusal: RegistrationRefused::InlineBudgetExceedsCeiling { .. }
        }
    ));
}
