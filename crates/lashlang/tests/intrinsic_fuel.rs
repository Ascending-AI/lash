//! Intrinsics charge their proportional work to the instruction budget.
//!
//! An intrinsic's dispatch is one instruction, but `JSON.parse` over a large
//! text is proportional work behind that one bytecode. The instruction budget
//! is the VM's only deterministic bound, so every intrinsic whose work grows
//! with its input or output charges one instruction per unit of that work.
//!
//! Each case below runs one intrinsic over a small input and a large one under
//! the same budget. The small run must finish, which shows the budget covers
//! the program around the call. The large run must exhaust the budget, which
//! it can only do if the intrinsic charged for its input: the two programs
//! differ only in the input's size. Add an intrinsic here when it gains a charge.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment,
    RuntimeError, State,
};

use crate::execute_support::{ExecuteError, execute};

/// The budget every case runs under: far above what the program around the
/// intrinsic spends, far below the large input's size.
const BUDGET: u64 = 5_000;
const SMALL: usize = 10;
const LARGE: usize = 50_000;

struct BudgetHost;

impl ExecutionHost for BudgetHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "no other host abilities in this test",
            )),
        }
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(
            ExecutionBound::instructions(BUDGET),
            ExecutionBound::Unbounded,
            ExecutionBounds::memory_bounded_default().memory_limit,
        )
    }
}

struct Case {
    intrinsic: &'static str,
    /// A cell that runs the intrinsic once over an input of `size` units.
    source: fn(usize) -> String,
}

const CASES: &[Case] = &[
    Case {
        intrinsic: "JSON.parse",
        source: |size| {
            format!(
                r#"const text = "[" + "1,".repeat({size}) + "1]"; finish(JSON.parse(text).length);"#
            )
        },
    },
    Case {
        intrinsic: "JSON.stringify",
        source: |size| {
            format!(r#"const text = "x".repeat({size}); finish(JSON.stringify(text).length);"#)
        },
    },
];

async fn run(source: &str) -> Result<ExecutionOutcome, ExecuteError> {
    let environment =
        LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all());
    execute(source, &mut State::new(), &BudgetHost, environment).await
}

#[tokio::test(flavor = "current_thread")]
async fn every_charged_intrinsic_spends_the_budget_in_proportion_to_its_input() {
    for case in CASES {
        let small = run(&(case.source)(SMALL)).await;
        assert!(
            matches!(small, Ok(ExecutionOutcome::Finished(_))),
            "{}: a small input must fit the budget, got {small:?}",
            case.intrinsic
        );
        let large = run(&(case.source)(LARGE)).await;
        assert!(
            matches!(
                large,
                Err(ExecuteError::Runtime(
                    RuntimeError::InstructionBudgetExceeded { limit: BUDGET }
                ))
            ),
            "{}: a large input must exhaust the budget, got {large:?}",
            case.intrinsic
        );
    }
}

/// The instruction a budget runs out on is a function of the program and its
/// input, so two runs of one cell report the same exhaustion.
#[tokio::test(flavor = "current_thread")]
async fn budget_exhaustion_is_identical_across_runs() {
    for case in CASES {
        let source = (case.source)(LARGE);
        let first = run(&source).await;
        let second = run(&source).await;
        assert_eq!(first, second, "{}", case.intrinsic);
    }
}
