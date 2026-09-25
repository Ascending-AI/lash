//! ECMA conformance regressions, one file per ticket (FIG-3727): a new
//! ticket's regressions land in `ecma_regressions/fig_<n>.rs`, so two lanes
//! never edit the same file. The shared host and runners stay here.
use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, RuntimeError, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new(
                "unsupported ECMA regression ability",
            )),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source)
        .unwrap_or_else(|error| panic!("TypeScript should execute: {source}: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[path = "ecma_regressions/fig_1304.rs"]
mod fig_1304;
#[path = "ecma_regressions/fig_1305.rs"]
mod fig_1305;
#[path = "ecma_regressions/fig_1413.rs"]
mod fig_1413;
#[path = "ecma_regressions/fig_2823.rs"]
mod fig_2823;
#[path = "ecma_regressions/fig_3646.rs"]
mod fig_3646;
#[path = "ecma_regressions/fig_3652.rs"]
mod fig_3652;
#[path = "ecma_regressions/fig_3653_and_fig_3654.rs"]
mod fig_3653_and_fig_3654;
#[path = "ecma_regressions/fig_3655.rs"]
mod fig_3655;
#[path = "ecma_regressions/fig_3656.rs"]
mod fig_3656;
#[path = "ecma_regressions/fig_3657.rs"]
mod fig_3657;
#[path = "ecma_regressions/fig_3662.rs"]
mod fig_3662;
#[path = "ecma_regressions/fig_3700.rs"]
mod fig_3700;
#[path = "ecma_regressions/fig_3701.rs"]
mod fig_3701;
#[path = "ecma_regressions/fig_3703.rs"]
mod fig_3703;
#[path = "ecma_regressions/fig_3704.rs"]
mod fig_3704;
#[path = "ecma_regressions/fig_3706.rs"]
mod fig_3706;
#[path = "ecma_regressions/fig_3707.rs"]
mod fig_3707;
#[path = "ecma_regressions/fig_3720.rs"]
mod fig_3720;
