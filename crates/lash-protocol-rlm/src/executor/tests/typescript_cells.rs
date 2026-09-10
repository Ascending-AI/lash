use super::*;

#[test]
fn parser_accepts_bounded_while_with_nested_for() {
    let source = r#"pool_i = 0
final_ids = []
candidate_pools = [{ matches: ["a", "b"] }]
while len(final_ids) < 2 && pool_i < len(candidate_pools) {
  for m in candidate_pools[pool_i].matches {
    final_ids = final_ids + [m]
  }
  pool_i = pool_i + 1
}
finish final_ids"#;

    lashlang::compile(source).expect("while should compile");
}

/// Closure-bearing TypeScript cells, mirroring the closure shapes of
/// `lash-typescript`'s durability corpus (`tests/dialect.rs`): a recursive
/// function, a nested function, an arrow that captures and is returned, and
/// an inline arrow whose closure becomes garbage immediately.
const CLOSURE_BEARING_TYPESCRIPT_CELLS: &[&str] = &[
    "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; } const f5 = fact(5);",
    "const top = 9; function outerFn(): number { function innerFn(): number { return top; } return innerFn(); } const nested = outerFn();",
    "const base = 10; const outer = () => { const inner = () => base; return inner; }; const held = outer();",
    "const xs = [1].map(x => x + 1);",
];

/// The trivial next cell from the FIG-1562 report.
const TRIVIAL_NEXT_CELL: &str = "finish(6 * 7);";

async fn execute_typescript_test_cell(
    mut state: RlmExecutionState,
    code: &str,
) -> (RlmExecutionState, ExecResponse) {
    let response = execute_code_with_dialect_and_bounds(
        &mut state,
        lash_core::testing::code_execution_context(),
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(SourceDialect::Typescript),
    )
    .await;
    (state, response)
}

/// A `console.log` observation has to describe the value the cell inspected.
///
/// The prompt tells the model to inspect values with `console.log`, and on
/// the pre-FIG-2767 path every one of these cells wrote `[object Object]`
/// into the observation, so 17 toolbench attempts learned nothing from the
/// step they were told to take. This is the end-to-end witness: the text
/// asserted here is what reaches the model.
#[test]
fn typescript_console_observations_describe_the_value() {
    block_on(async {
        for (cell, expected) in [
            (
                "const record = { id: 7, tags: [\"a\", \"b\"] }; console.log(record);",
                r#"{"id":7,"tags":["a","b"]}"#,
            ),
            (
                "console.log(\"record:\", { id: 7 });",
                r#"record: {"id":7}"#,
            ),
            (
                "console.log([{ id: 1 }, { id: 2 }]);",
                r#"[{"id":1},{"id":2}]"#,
            ),
        ] {
            let state = RlmExecutionState::for_engine("typescript");
            let (_, response) = execute_typescript_test_cell(state, cell).await;
            assert!(
                response.error.is_none(),
                "cell `{cell}`: {:?}",
                response.error
            );
            let observations = response
                .observations
                .iter()
                .map(|observation| observation.text.as_str())
                .collect::<Vec<_>>();
            assert_eq!(observations, vec![expected], "cell `{cell}`");
            assert!(
                !observations
                    .iter()
                    .any(|text| text.contains("[object Object]")),
                "cell `{cell}` still renders an opaque object"
            );
        }
    });
}

/// A closure allocated by one cell must not fail validation of the next
/// cell's program.
///
/// Each RLM cell compiles its own `CompiledProgram` while the heap survives
/// the cell boundary, so a closure from cell N is re-validated against cell
/// N+1's function table. See FIG-1562.
#[test]
fn a_closure_from_one_typescript_cell_does_not_poison_the_next_cell() {
    block_on(async {
        for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
            let state = RlmExecutionState::for_engine("typescript");
            let (state, first) = execute_typescript_test_cell(state, cell).await;
            assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

            let (_, second) = execute_typescript_test_cell(state, TRIVIAL_NEXT_CELL).await;
            assert!(
                second.error.is_none(),
                "the trivial cell after `{cell}` failed: {:?}",
                second.error
            );
        }
    });
}

/// The same composition across the durability boundary: snapshot a state
/// holding a real closure, restore it into a fresh engine, then run a
/// *different* cell against it.
///
/// This one passes today, and the sibling test above is why it is worth
/// keeping: the closure demonstrably survives the cell boundary in memory,
/// so the fact that a *restored* state accepts the next cell is a real
/// property of the RLM persistence path (closure-valued globals do not
/// reach the snapshot), not an artefact of an empty heap. It guards that
/// property once the cell boundary itself is fixed. See FIG-1562.
#[test]
fn a_restored_typescript_closure_does_not_poison_a_different_cell() {
    block_on(async {
        for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
            let state = RlmExecutionState::for_engine("typescript");
            let (mut state, first) = execute_typescript_test_cell(state, cell).await;
            assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

            let snapshot = hydrate_snapshot(
                state
                    .snapshot_execution_state()
                    .expect("snapshot components"),
            );
            let mut restored = RlmExecutionState::for_engine("typescript");
            restored
                .restore_execution_state(&snapshot)
                .expect("restore TypeScript execution state");

            let (_, response) = execute_typescript_test_cell(restored, TRIVIAL_NEXT_CELL).await;
            assert!(
                response.error.is_none(),
                "the trivial cell after restoring `{cell}` failed: {:?}",
                response.error
            );
        }
    });
}
