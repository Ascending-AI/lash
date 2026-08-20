use compact_str::ToCompactString;
use lashlang::{
    AbilityOp, AbilityResult, AssignTarget, BinaryOp, ExecutionHost, ExecutionHostError, Expr,
    FunctionExpr, HostDescriptor, ImageValue, LASH_PROCESS_NAME_KEY, LashlangAbilities,
    LashlangHostCatalog, LashlangHostEnvironment, LinkedModule, ListValue, Program,
    ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, Record, State, TypeExpr, TypeField, Value, from_json,
};
use std::fmt;
use std::sync::{Arc, OnceLock};

macro_rules! scenarios {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $variant:ident => $str:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug)]
        $vis enum $name {
            $( $variant, )*
        }

        #[allow(dead_code)]
        impl $name {
            pub const ALL: &'static [Self] = &[
                $( Self::$variant, )*
            ];

            pub fn parse(value: &str) -> Option<Self> {
                Some(match value {
                    $( $str => Self::$variant, )*
                    _ => return None,
                })
            }

            pub fn expected_values() -> &'static str {
                concat!($( $str, ", ", )* "or all")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self {
                    $( Self::$variant => $str, )*
                })
            }
        }
    };
}

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

scenarios! {
    #[allow(dead_code)]
    pub enum FunctionScenario {
        NonCapturingCall => "function_call_noncapturing",
        CapturedCall => "function_call_captured",
        DeepRecursion => "function_deep_recursion",
        Map64 => "function_map_64",
        Map256 => "function_map_256",
        Map1024 => "function_map_1024",
        FrameHeavy => "function_frame_heavy",
    }
}

#[allow(dead_code)]
fn ast_variable(name: &str) -> Expr {
    Expr::Variable(name.into())
}

#[allow(dead_code)]
fn ast_assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

#[allow(dead_code)]
fn ast_call(function: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call {
        function: Box::new(function),
        args,
    }
}

#[allow(dead_code)]
fn ast_function(name: Option<&str>, params: &[&str], captures: &[&str], body: Expr) -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: name.map(Into::into),
        params: params.iter().map(|name| (*name).into()).collect(),
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

/// Builds the AST-only programs that guard function, frame, and map costs.
#[allow(dead_code)]
pub fn function_benchmark_program(scenario: FunctionScenario) -> Program {
    match scenario {
        FunctionScenario::NonCapturingCall => Program::block(vec![
            ast_assign(
                "increment",
                ast_function(
                    None,
                    &["value"],
                    &[],
                    Expr::Binary {
                        left: Box::new(ast_variable("value")),
                        op: BinaryOp::Add,
                        right: Box::new(Expr::Number(1.0)),
                    },
                ),
            ),
            Expr::Finish(Box::new(ast_call(
                ast_variable("increment"),
                vec![Expr::Number(41.0)],
            ))),
        ]),
        FunctionScenario::CapturedCall => Program::block(vec![
            ast_assign("offset", Expr::List(vec![Expr::Number(1.0)])),
            ast_assign(
                "increment",
                ast_function(
                    None,
                    &["value"],
                    &["offset"],
                    Expr::Binary {
                        left: Box::new(ast_variable("value")),
                        op: BinaryOp::Add,
                        right: Box::new(Expr::Index {
                            target: Box::new(ast_variable("offset")),
                            index: Box::new(Expr::Number(0.0)),
                        }),
                    },
                ),
            ),
            Expr::Finish(Box::new(ast_call(
                ast_variable("increment"),
                vec![Expr::Number(41.0)],
            ))),
        ]),
        FunctionScenario::DeepRecursion | FunctionScenario::FrameHeavy => {
            let terminal = if matches!(scenario, FunctionScenario::FrameHeavy) {
                Expr::Yield(Box::new(Expr::List(vec![Expr::Number(0.0); 8])))
            } else {
                Expr::Number(0.0)
            };
            let recurse = ast_call(
                ast_variable("countdown"),
                vec![Expr::Binary {
                    left: Box::new(ast_variable("n")),
                    op: BinaryOp::Subtract,
                    right: Box::new(Expr::Number(1.0)),
                }],
            );
            let depth = if matches!(scenario, FunctionScenario::FrameHeavy) {
                512.0
            } else {
                768.0
            };
            Program::block(vec![
                ast_assign(
                    "countdown",
                    ast_function(
                        Some("countdown"),
                        &["n"],
                        &[],
                        Expr::If {
                            condition: Box::new(Expr::Binary {
                                left: Box::new(ast_variable("n")),
                                op: BinaryOp::LessEqual,
                                right: Box::new(Expr::Number(0.0)),
                            }),
                            then_block: Box::new(terminal),
                            else_block: Box::new(recurse),
                        },
                    ),
                ),
                Expr::Finish(Box::new(ast_call(
                    ast_variable("countdown"),
                    vec![Expr::Number(depth)],
                ))),
            ])
        }
        FunctionScenario::Map64 | FunctionScenario::Map256 | FunctionScenario::Map1024 => {
            let size = match scenario {
                FunctionScenario::Map64 => 64,
                FunctionScenario::Map256 => 256,
                FunctionScenario::Map1024 => 1_024,
                _ => unreachable!("map arm only receives map scenarios"),
            };
            Program::block(vec![
                ast_assign(
                    "increment",
                    ast_function(
                        None,
                        &["value"],
                        &[],
                        Expr::Binary {
                            left: Box::new(ast_variable("value")),
                            op: BinaryOp::Add,
                            right: Box::new(Expr::Number(1.0)),
                        },
                    ),
                ),
                Expr::Finish(Box::new(Expr::Map {
                    items: Box::new(Expr::List(
                        (0..size).map(|value| Expr::Number(value as f64)).collect(),
                    )),
                    function: Box::new(ast_variable("increment")),
                })),
            ])
        }
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
