use compact_str::ToCompactString;
use lashlang::{
    AbilityOp, AbilityOutcome, CoercingBinaryOp, CoercingUnaryOp, Declaration, ExecutionHost,
    ExecutionHostError, Expr, HostDescriptor, ImageValue, LashlangAbilities, LashlangHostCatalog,
    LashlangHostEnvironment, LinkedModule, ListValue, OperandLogicalOp, Program, ProjectedBindings,
    ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record,
    State, TypeExpr, TypeField, Value, from_json,
};
use std::fmt;
use std::sync::{Arc, OnceLock};

#[macro_use]
pub mod functions;

scenarios! {
    pub enum Scenario {
        Baseline => "baseline",
        LanguageHostEnvironment => "language_host_environment",
        AsyncAwait => "async_await",
        DirectUnwrap => "direct_unwrap",
        GeneralFanout => "general_fanout",
        LoopControl => "loop_control",
        IndexedAssignment => "indexed_assignment",
        ProjectedValues => "projected_values",
        LargeData => "large_data",
        CachePressure => "cache_pressure",
        ProjectedOperations => "projected_operations",
        TypeSystemStress => "type_system_stress",
        WrappedErrorPaths => "wrapped_error_paths",
        ToolControlHostEnvironment => "tool_control_host_environment",
        SnapshotProjectedState => "snapshot_projected_state",
        ContinueAsSeedHostEnvironment => "continue_as_seed_host_environment",
        TriggerRegistryHostEnvironment => "trigger_registry_host_environment",
        SyntaxTextHostEnvironment => "syntax_text_host_environment",
        IntegerRangeHostEnvironment => "integer_range_host_environment",
        FanoutExpressionHostEnvironment => "fanout_expression_host_environment",
        ImageHostEnvironment => "image_host_environment",
        HeapListIteration => "heap_list_iteration",
        HeapNestedLoop => "heap_nested_loop",
        HeapAllocationChurn => "heap_allocation_churn",
        HeapDeepChainMutation => "heap_deep_chain_mutation",
        HeapComprehensionBuild => "heap_comprehension_build",
        HeapVariableConcat => "heap_variable_concat",
        HeapShallowChainMutation => "heap_shallow_chain_mutation",
        HeapDeepChainMutation24 => "heap_deep_chain_mutation_24",
    }
}

#[allow(dead_code)]
pub fn seeded_state() -> State {
    seeded_state_for(Scenario::Baseline)
}

#[expect(
    clippy::unwrap_used,
    reason = "the canonical benchmark seeds a literal image/png attachment, which parses by construction"
)]
pub fn seeded_state_for(scenario: Scenario) -> State {
    let mut globals = Record::default();
    globals.insert(
        "history".to_string(),
        Value::List(
            vec![
                Value::String("alpha".to_string().into()),
                Value::String("beta".to_string().into()),
                Value::String("gamma".to_string().into()),
            ]
            .into(),
        ),
    );
    globals.insert(
        "ctx".to_string(),
        Value::Record({
            let mut record = Record::default();
            record.insert("user".to_string(), Value::String("sam".into()));
            record.insert("attempt".to_string(), Value::Number(3.0));
            record.into()
        }),
    );
    if matches!(scenario, Scenario::SnapshotProjectedState) {
        globals.insert("snap".to_string(), snapshot_projected_record());
    }
    if matches!(scenario, Scenario::ImageHostEnvironment) {
        globals.insert(
            "img".to_string(),
            Value::Image(Box::new(ImageValue::new(
                "img-1",
                lashlang::MediaType::parse("image/png").unwrap(),
                "chart.png",
                1234,
                Some(640),
                Some(480),
            ))),
        );
    }
    State::from_snapshot(lashlang::Snapshot::new(globals))
}

pub mod builders;

use self::builders as b;

include!("sections/program.rs");
include!("sections/environment.rs");
include!("sections/projected.rs");
include!("sections/host.rs");
