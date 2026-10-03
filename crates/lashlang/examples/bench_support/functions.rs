use lashlang::{
    AbilityOp, AbilityOutcome, AssignTarget, CoercingBinaryOp, ExecutionHost, ExecutionHostError,
    ExecutionMode, Expr, FunctionExpr, Program, Value,
};
use std::fmt;

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
pub struct FrameHost;

impl ExecutionHost for FrameHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected frame benchmark effect")),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
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
        js_name: None,
        receiver: None,
        params: params.iter().map(|name| (*name).into()).collect(),
        captures: captures.iter().map(|name| (*name).into()).collect(),
        body: Box::new(body),
    }))
}

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
                    Expr::CoercingBinary {
                        left: Box::new(ast_variable("value")),
                        op: CoercingBinaryOp::Add,
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
                    Expr::CoercingBinary {
                        left: Box::new(ast_variable("value")),
                        op: CoercingBinaryOp::Add,
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
                Expr::Block(vec![
                    ast_assign("payload", Expr::List(vec![Expr::Number(0.0); 8])),
                    Expr::SleepFor(Box::new(Expr::Number(0.0))),
                    ast_variable("payload"),
                ])
            } else {
                Expr::Number(0.0)
            };
            let recurse = ast_call(
                ast_variable("countdown"),
                vec![Expr::CoercingBinary {
                    left: Box::new(ast_variable("n")),
                    op: CoercingBinaryOp::Subtract,
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
                            condition: Box::new(Expr::CoercingBinary {
                                left: Box::new(ast_variable("n")),
                                op: CoercingBinaryOp::LessEqual,
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
                        Expr::CoercingBinary {
                            left: Box::new(ast_variable("value")),
                            op: CoercingBinaryOp::Add,
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
