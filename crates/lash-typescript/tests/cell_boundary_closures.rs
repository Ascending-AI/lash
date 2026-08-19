//! A closure allocated by one RLM cell must not fail the next cell.
//!
//! Each cell of an RLM session compiles its own `CompiledProgram` while the
//! `State` — heap included — carries over. Closure function indices are
//! program-scoped, so a closure that outlives its program is judged against a
//! function table that never compiled it: the next cell fails at validation
//! with `UnknownFunction` or `ClosureCaptureCountMismatch`, whatever it says.
//! On a live host this poisoned a durable session for every subsequent cell,
//! including `finish(6 * 7);`.
//!
//! Two ways a closure reached the next cell, and both are pinned here: as heap
//! garbage the boundary never collected, and as a live root in the session
//! globals.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected cell-boundary ability")),
        }
    }
}

/// Runs `source` as the next cell of the session `state` carries.
///
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
    let program =
        lashlang::compile_ast_with_dialect(&ast, lashlang::CompilationDialect::Typescript)
            .unwrap_or_else(|error| panic!("cell `{source}` should compile: {error}"));
    futures::executor::block_on(lashlang::execute(&program, state, &Host))
        .unwrap_or_else(|error| panic!("cell `{source}` should execute: {error}"))
}

#[test]
fn a_dead_closure_from_an_earlier_cell_does_not_fail_the_next_one() {
    let mut state = State::new();
    // The arrow lives only for the duration of the `map` call, so nothing
    // roots it once the cell ends — but it stayed resident on the heap, and
    // validation judged it anyway.
    run_cell(&mut state, "const xs = [1].map(x => x + 1);");
    assert_eq!(
        run_cell(&mut state, "finish(6 * 7);"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
}

#[test]
fn a_closure_bound_to_a_session_global_does_not_fail_the_next_cell() {
    let mut state = State::new();
    // Here the closure is reachable, so collecting the heap cannot remove it:
    // the binding itself has to go, exactly as the exported view of the
    // globals already drops any global that reaches a function value.
    run_cell(
        &mut state,
        "const add = (x: number) => x + 1;\nconst y = add(1);",
    );
    assert_eq!(
        run_cell(&mut state, "finish(6 * 7);"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    // The non-closure binding from that cell is untouched: only the function
    // value is program-scoped.
    assert_eq!(
        state.globals().get("y"),
        Some(&Value::Number(2.0)),
        "dropping a closure global must not disturb the rest of the session"
    );
}

#[test]
fn a_closure_nested_inside_a_session_global_does_not_fail_the_next_cell() {
    let mut state = State::new();
    // A closure reaches the next cell just as well from inside a container, so
    // the boundary check is a reachability question, not a shallow type test.
    run_cell(
        &mut state,
        "const handlers = { onDone: (x: number) => x + 1 };\nconst tag = \"kept\";",
    );
    assert_eq!(
        run_cell(&mut state, "finish(6 * 7);"),
        ExecutionOutcome::Finished(Value::Number(42.0))
    );
    assert_eq!(
        state.globals().get("tag"),
        Some(&Value::String("kept".into()))
    );
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
