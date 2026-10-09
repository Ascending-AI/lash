//! Ordinary session values cross the cell boundary; function-valued globals do not.

use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityOutcome::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected cell-boundary ability")),
        }
    }
}

/// The cell is lowered against the session's live globals and compiled on its
/// own, which is what RLM does per cell: one fresh program, one surviving
/// state.
fn run_cell(state: &mut State, source: &str) -> ExecutionOutcome {
    let globals = state
        .globals()
        .keys()
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let ast = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("cell `{source}` should lower: {error}"));
    let program = lash_vm::testing::harness::try_compile_program(&ast)
        .unwrap_or_else(|error| panic!("cell `{source}` should compile: {error}"));
    futures::executor::block_on(lash_vm::execute(&program, state, &Host))
        .unwrap_or_else(|error| panic!("cell `{source}` should execute: {error}"))
}

#[test]
fn ordinary_session_state_still_crosses_the_cell_boundary() {
    // The boundary drops closures, not values: a cell still reads what an
    // earlier cell bound, which is the whole point of a durable session.
    let mut state = State::new();
    run_cell(&mut state, "const xs = [1, 2, 3].map(x => x * 2);");
    assert_eq!(
        run_cell(&mut state, "finish(xs[2]);"),
        ExecutionOutcome::Finished(Value::Number(6.0))
    );
}

#[test]
fn a_builtin_method_value_bound_to_a_session_global_does_not_survive_the_cell() {
    // `'x'.includes` is a function like an arrow is: it works in its own cell
    // and the binding is dropped at the boundary (FIG-3701).
    let mut state = State::new();
    run_cell(
        &mut state,
        "const has = 'x'.includes;\nconst box = { find: [].indexOf, n: 2 };\nconst tag = typeof has;",
    );
    assert_eq!(
        run_cell(&mut state, "finish(6 * 7);"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    assert_eq!(state.globals().get("has"), None);
    assert_eq!(state.globals().get("box"), None);
    assert_eq!(
        state.globals().get("tag"),
        Some(&Value::String("function".into()))
    );
}
