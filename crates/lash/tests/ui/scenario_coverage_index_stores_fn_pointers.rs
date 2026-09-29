// ADR 0007: every scenario coverage index entry names its test through a
// function pointer, so deleting or renaming a listed test fails compilation
// instead of leaving a stale `stringify!` name behind. This fixture mirrors
// the coverage-entry shape and names a test that does not exist.

#[derive(Clone, Copy, Debug)]
struct RuntimeScenarioCoverage {
    test_name: &'static str,
    declared_test: fn(),
    display_name: &'static str,
    owned_invariant: &'static str,
}

macro_rules! runtime_scenario_coverage {
    ($test_fn:ident, $display_name:literal, $owned_invariant:literal) => {
        RuntimeScenarioCoverage {
            test_name: stringify!($test_fn),
            declared_test: $test_fn,
            display_name: $display_name,
            owned_invariant: $owned_invariant,
        }
    };
}

const DELETED_SCENARIO: RuntimeScenarioCoverage = runtime_scenario_coverage!(
    runtime_scenario_test_that_was_deleted,
    "deleted scenario",
    "The test this entry names has been deleted."
);

fn main() {
    let _ = DELETED_SCENARIO;
}
