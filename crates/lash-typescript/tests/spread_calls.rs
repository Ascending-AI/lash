//! Spread arguments to builtin functions and methods (FIG-3627).

use lashlang::testing::harness::test_environment;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new(
                "unexpected ability in a spread law",
            )),
        }
    }
}

fn run_typescript(source: &str) -> Value {
    let linked = lash_typescript::link(source, &test_environment())
        .unwrap_or_else(|error| panic!("link `{source}`: {error}"));
    match futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &Host,
    ))
    .unwrap_or_else(|error| panic!("execute `{source}`: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// A spread argument to a builtin function or method is passed as ECMA-262
/// specifies (FIG-3627): its items become the call's trailing arguments, alone
/// or among fixed ones, into a static function, a method, and a spread nested
/// inside another spread's items. Every answer is Node's.
#[test]
fn spread_arguments_reach_builtins_as_ecma_specifies() {
    for (source, node) in [
        (
            "const xs = [3, 1, 7]; finish(String(Math.max(...xs)));",
            "7",
        ),
        (
            "const xs = [3, 1, 7]; finish(String(Math.min(...xs, 0)));",
            "0",
        ),
        ("finish(String(Math.max(...[])));", "-Infinity"),
        (
            "const items = [3, 1]; items.push(...[4, 5]); finish(JSON.stringify(items));",
            "[3,1,4,5]",
        ),
        (
            "const items = [3]; const n = items.push(...[4], 5, ...[6, 7]); finish(`${n}|${JSON.stringify(items)}`);",
            "5|[3,4,5,6,7]",
        ),
        (
            "const items = [1]; items.unshift(...['a', 'b']); finish(JSON.stringify(items));",
            "[\"a\",\"b\",1]",
        ),
        (
            "const codes = [104, 105]; finish(String.fromCharCode(...codes));",
            "hi",
        ),
        (
            "const parts = [[1, 2], [3]]; finish(JSON.stringify([0].concat(...parts)));",
            "[0,1,2,3]",
        ),
        (
            "const nested = [[5, 9], [2]]; finish(String(Math.max(...nested.map((xs) => Math.max(...xs)))));",
            "9",
        ),
        (
            "const o = { k: 1 }; const items = []; items.push(...[o]); finish(String(items[0] === o));",
            "true",
        ),
        (
            "const xs = ['b', 'a']; const joined = []; joined.push(...xs.sort()); finish(joined.join(','));",
            "a,b",
        ),
        (
            "const target = [1, 2, 3, 4]; target.splice(1, 2, ...['x', 'y', 'z']); finish(JSON.stringify(target));",
            "[1,\"x\",\"y\",\"z\",4]",
        ),
        (
            "const words = ['a', 'b']; finish('x'.concat(...words));",
            "xab",
        ),
        ("finish(JSON.stringify(Array.of(...[1, 2])));", "[1,2]"),
    ] {
        assert_eq!(
            run_typescript(source),
            Value::String(node.into()),
            "{source}"
        );
    }
}

/// A builtin whose lowering depends on its argument count refuses a spread
/// argument by name rather than faulting when it runs.
#[test]
fn a_spread_into_a_count_dependent_builtin_is_refused_by_name() {
    let environment = test_environment();
    for source in [
        "const fs = [(x: number) => x]; finish([1].map(...fs).length);",
        "const values = [1]; finish(...values);",
    ] {
        let error = lash_typescript::link(source, &environment)
            .expect_err("a count-dependent builtin refuses a spread argument");
        assert_eq!(error.code.as_str(), "TS_METHOD_UNSUPPORTED", "{source}");
        assert!(
            error.to_string().contains("a spread argument to"),
            "{source}: {error}"
        );
    }
}
