use compact_str::ToCompactString;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, HostDescriptor, ImageValue,
    LASH_PROCESS_NAME_KEY, LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment,
    LinkedModule, ListValue, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
    ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record, State, TypeExpr,
    TypeField, Value, from_json,
};
use std::fmt;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    Baseline,
    LanguageHostEnvironment,
    AsyncAwait,
    DirectUnwrap,
    GeneralFanout,
    LoopControl,
    IndexedAssignment,
    ProjectedValues,
    LargeData,
    CachePressure,
    ProjectedOperations,
    TypeSystemStress,
    WrappedErrorPaths,
    ToolControlHostEnvironment,
    SnapshotProjectedState,
    ContinueAsSeedHostEnvironment,
    TriggerRegistryHostEnvironment,
    SyntaxTextHostEnvironment,
    IntegerRangeHostEnvironment,
    FanoutExpressionHostEnvironment,
    ImageHostEnvironment,
    HeapListIteration,
    HeapNestedLoop,
    HeapAllocationChurn,
    HeapDeepChainMutation,
    HeapComprehensionBuild,
}

impl Scenario {
    pub const ALL: &'static [Self] = &[
        Self::Baseline,
        Self::LanguageHostEnvironment,
        Self::AsyncAwait,
        Self::DirectUnwrap,
        Self::GeneralFanout,
        Self::LoopControl,
        Self::IndexedAssignment,
        Self::ProjectedValues,
        Self::LargeData,
        Self::CachePressure,
        Self::ProjectedOperations,
        Self::TypeSystemStress,
        Self::WrappedErrorPaths,
        Self::ToolControlHostEnvironment,
        Self::SnapshotProjectedState,
        Self::ContinueAsSeedHostEnvironment,
        Self::TriggerRegistryHostEnvironment,
        Self::SyntaxTextHostEnvironment,
        Self::IntegerRangeHostEnvironment,
        Self::FanoutExpressionHostEnvironment,
        Self::ImageHostEnvironment,
        Self::HeapListIteration,
        Self::HeapNestedLoop,
        Self::HeapAllocationChurn,
        Self::HeapDeepChainMutation,
        Self::HeapComprehensionBuild,
    ];

    #[allow(dead_code)]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "baseline" => Self::Baseline,
            "language_host_environment" => Self::LanguageHostEnvironment,
            "async_await" => Self::AsyncAwait,
            "direct_unwrap" => Self::DirectUnwrap,
            "general_fanout" => Self::GeneralFanout,
            "loop_control" => Self::LoopControl,
            "indexed_assignment" => Self::IndexedAssignment,
            "projected_values" => Self::ProjectedValues,
            "large_data" => Self::LargeData,
            "cache_pressure" => Self::CachePressure,
            "projected_operations" => Self::ProjectedOperations,
            "type_system_stress" => Self::TypeSystemStress,
            "wrapped_error_paths" => Self::WrappedErrorPaths,
            "tool_control_host_environment" => Self::ToolControlHostEnvironment,
            "snapshot_projected_state" => Self::SnapshotProjectedState,
            "continue_as_seed_host_environment" => Self::ContinueAsSeedHostEnvironment,
            "trigger_registry_host_environment" => Self::TriggerRegistryHostEnvironment,
            "syntax_text_host_environment" => Self::SyntaxTextHostEnvironment,
            "integer_range_host_environment" => Self::IntegerRangeHostEnvironment,
            "fanout_expression_host_environment" => Self::FanoutExpressionHostEnvironment,
            "image_host_environment" => Self::ImageHostEnvironment,
            "heap_list_iteration" => Self::HeapListIteration,
            "heap_nested_loop" => Self::HeapNestedLoop,
            "heap_allocation_churn" => Self::HeapAllocationChurn,
            "heap_deep_chain_mutation" => Self::HeapDeepChainMutation,
            "heap_comprehension_build" => Self::HeapComprehensionBuild,
            _ => return None,
        })
    }

    #[allow(dead_code)]
    pub fn expected_values() -> &'static str {
        "baseline, language_host_environment, async_await, direct_unwrap, general_fanout, loop_control, indexed_assignment, projected_values, large_data, cache_pressure, projected_operations, type_system_stress, wrapped_error_paths, tool_control_host_environment, snapshot_projected_state, continue_as_seed_host_environment, trigger_registry_host_environment, syntax_text_host_environment, integer_range_host_environment, fanout_expression_host_environment, image_host_environment, heap_list_iteration, heap_nested_loop, heap_allocation_churn, heap_deep_chain_mutation, heap_comprehension_build, or all"
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Baseline => "baseline",
            Self::LanguageHostEnvironment => "language_host_environment",
            Self::AsyncAwait => "async_await",
            Self::DirectUnwrap => "direct_unwrap",
            Self::GeneralFanout => "general_fanout",
            Self::LoopControl => "loop_control",
            Self::IndexedAssignment => "indexed_assignment",
            Self::ProjectedValues => "projected_values",
            Self::LargeData => "large_data",
            Self::CachePressure => "cache_pressure",
            Self::ProjectedOperations => "projected_operations",
            Self::TypeSystemStress => "type_system_stress",
            Self::WrappedErrorPaths => "wrapped_error_paths",
            Self::ToolControlHostEnvironment => "tool_control_host_environment",
            Self::SnapshotProjectedState => "snapshot_projected_state",
            Self::ContinueAsSeedHostEnvironment => "continue_as_seed_host_environment",
            Self::TriggerRegistryHostEnvironment => "trigger_registry_host_environment",
            Self::SyntaxTextHostEnvironment => "syntax_text_host_environment",
            Self::IntegerRangeHostEnvironment => "integer_range_host_environment",
            Self::FanoutExpressionHostEnvironment => "fanout_expression_host_environment",
            Self::ImageHostEnvironment => "image_host_environment",
            Self::HeapListIteration => "heap_list_iteration",
            Self::HeapNestedLoop => "heap_nested_loop",
            Self::HeapAllocationChurn => "heap_allocation_churn",
            Self::HeapDeepChainMutation => "heap_deep_chain_mutation",
            Self::HeapComprehensionBuild => "heap_comprehension_build",
        })
    }
}

#[allow(dead_code)]
pub fn seeded_state() -> State {
    seeded_state_for(Scenario::Baseline)
}

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

include!("sections/program.rs");
include!("sections/environment.rs");
include!("sections/projected.rs");
include!("sections/host.rs");
