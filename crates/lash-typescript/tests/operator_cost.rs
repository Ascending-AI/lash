//! FIG-3730: ECMA-262's bitwise and shift operators each run as one VM
//! instruction, the same cost as the arithmetic operator of the same arity.
//! They used to lower to an unrolled 32-bit expansion costing about 2,800
//! instructions per `&`, which put test262's exhaustive URI tests past the
//! instruction budget.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "the operator-cost host answers no effect",
            )),
        }
    }
}

/// The VM instructions a loop of 64 iterations of `x = <expression>` runs.
fn loop_instructions(expression: &str) -> u64 {
    let source = format!(
        "let x = 0; for (let i = 0; i < 64; i++) {{ x = {expression}; }} finish(typeof x);"
    );
    let program = lash_typescript::testing::compile(&source)
        .unwrap_or_else(|error| panic!("`{expression}` compiles: {error}"));
    let environment = ExecutionEnvironment::new(&Host).profiled();
    let outcome =
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &environment))
            .unwrap_or_else(|error| panic!("`{expression}` runs: {error}"));
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("number".into())),
        "`{expression}`"
    );
    environment
        .take_profile()
        .unwrap_or_else(|| panic!("`{expression}`: a profiled run reports its profile"))
        .instruction_stats()
        .iter()
        .map(|stat| stat.count)
        .sum()
}

#[test]
fn bitwise_and_shift_operators_cost_what_arithmetic_costs() {
    let binary = loop_instructions("i - 3");
    for expression in ["i & 3", "i | 3", "i ^ 3", "i << 3", "i >> 3", "i >>> 3"] {
        assert_eq!(
            loop_instructions(expression),
            binary,
            "`{expression}` runs as many instructions as `i - 3`"
        );
    }
    assert_eq!(
        loop_instructions("~i"),
        loop_instructions("-i"),
        "`~i` runs as many instructions as `-i`"
    );
}
