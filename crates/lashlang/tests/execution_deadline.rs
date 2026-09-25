//! FIG-3734: the execution deadline bounds wall time, whatever instructions
//! spend it.
//!
//! The deadline used to count only the time spent inside intrinsic
//! instructions: the loop restarted its clock before every instruction, so a
//! loop of plain instructions (arithmetic, loads, jumps) added about one
//! instruction's time per cooperative yield and ran for about a thousand
//! times the deadline before it tripped. The fuel budget here is far larger
//! than the loop can spend inside the bound, so only the deadline can stop it.

use std::time::{Duration, Instant};

use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionHost, ExecutionHostError,
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, RuntimeError, State,
};

use crate::execute_support::{ExecuteError, execute};

const DEADLINE: Duration = Duration::from_millis(50);

/// How far past the deadline the typed error may land: the loop reads the
/// clock once per cooperative-yield budget of instructions, which is
/// microseconds of work, and the rest is scheduling slack on a busy runner.
const SLACK: Duration = Duration::from_secs(2);

struct DeadlineHost;

impl ExecutionHost for DeadlineHost {
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
            ExecutionBound::instructions(u64::MAX / 2),
            ExecutionBound::Bounded(DEADLINE),
            ExecutionBounds::memory_bounded_default().memory_limit,
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_instruction_loop_stops_at_the_deadline() {
    let started = Instant::now();
    let error = execute(
        "let i = 0; while (true) { i = i + 1; }",
        &mut State::new(),
        &DeadlineHost,
        LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all()),
    )
    .await
    .expect_err("an endless loop cannot finish");
    let elapsed = started.elapsed();
    assert_eq!(
        error,
        ExecuteError::Runtime(RuntimeError::ExecutionDeadlineExceeded {
            limit_ms: DEADLINE.as_millis(),
        })
    );
    assert!(
        elapsed >= DEADLINE,
        "the deadline cannot trip before it has elapsed: {elapsed:?}"
    );
    assert!(
        elapsed < DEADLINE + SLACK,
        "the deadline stopped the loop {elapsed:?} after it started, past {DEADLINE:?} plus {SLACK:?}"
    );
}
