//! ECMA coercion of a *projected* host binding.
//!
//! A session global supplied by the host is a `Value::Projected`: a lazy handle
//! the runtime reads through instead of a materialized tree. Every non-path
//! operation is supposed to strip that wrapper and evaluate the value behind
//! it — that is what `Binary` (the Lashlang arithmetic opcode) does by routing
//! a projected operand to the async materializing path.
//!
//! The TypeScript opcodes `JavaScriptUnary`/`JavaScriptBinary` did not. A
//! projected operand fell straight into the scalar ECMA coercions, whose
//! `Value::Projected` arm was a `debug_assert` plus a fallback: debug builds
//! panicked out of the turn, and release builds silently carried
//! `"[object Object]"` (or `NaN`, or `false`) for a value whose ECMA result is
//! the projected string, number, or comparison. `console.log(item.kind, item.id)`
//! over a projected history — which lowers to `"" + a + " " + b` — is how a
//! workbench turn died (FIG-1446).
//!
//! The rule pinned here: a projected binding coerces exactly as the value
//! behind it does, in both build profiles, whether it reaches the coercion as
//! an operand of a TypeScript operator, as an argument to a stdlib call, or as
//! an element of a container the guest built from it.

use std::collections::BTreeSet;
use std::sync::Mutex;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, ProjectedValue,
    RuntimeError, State, Value,
};

#[derive(Default)]
struct Host {
    printed: Mutex<Vec<String>>,
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(value) => {
                self.printed
                    .lock()
                    .expect("print log is not poisoned")
                    .push(match value {
                        Value::String(value) => value.to_string(),
                        other => format!("{other:?}"),
                    });
                Ok(AbilityResult::Value(Value::Null))
            }
            _ => Err(ExecutionHostError::new(
                "unsupported projected-coercion ability",
            )),
        }
    }
}

/// Compiles `source` as a cell of a session whose `text` and `count` globals are
/// projected host bindings, then runs it.
async fn execute(source: &str) -> Result<(ExecutionOutcome, Vec<String>), RuntimeError> {
    let globals = BTreeSet::from(["text".to_string(), "count".to_string()]);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let program =
        lashlang::compile_ast_with_dialect(&program, lashlang::CompilationDialect::Typescript)
            .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let mut state = State::new();
    state
        .insert_global(
            "text",
            Value::Projected(ProjectedValue::scalar(
                "text",
                Value::String("hello".into()),
            )),
        )
        .expect("projected string global");
    state
        .insert_global(
            "count",
            Value::Projected(ProjectedValue::scalar("count", Value::Number(41.0))),
        )
        .expect("projected number global");
    let host = Host::default();
    let outcome = lashlang::execute(&program, &mut state, &host).await?;
    let printed = host
        .printed
        .into_inner()
        .expect("print log is not poisoned");
    Ok((outcome, printed))
}

async fn finished(source: &str) -> Value {
    match execute(source)
        .await
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"))
        .0
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("`{source}` should finish: {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_bindings_coerce_to_their_ecma_string() {
    for (source, expected) in [
        (r#"finish("content: " + text);"#, "content: hello"),
        (r#"finish(text + "!");"#, "hello!"),
        (r#"finish(`[${text}]`);"#, "[hello]"),
        (r#"finish(String(text));"#, "hello"),
        (r#"finish(text.toUpperCase());"#, "HELLO"),
        (r#"finish([text, "world"].join("|"));"#, "hello|world"),
        (r#"finish("n=" + count);"#, "n=41"),
        (r#"finish(String(count));"#, "41"),
        (r#"finish(typeof text);"#, "string"),
        (r#"finish(typeof count);"#, "number"),
    ] {
        assert_eq!(
            finished(source).await,
            Value::String(expected.into()),
            "{source}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_bindings_coerce_to_their_ecma_number() {
    for (source, expected) in [
        (r#"finish(count + 1);"#, 42.0),
        (r#"finish(count - 1);"#, 40.0),
        (r#"finish(count * 2);"#, 82.0),
        (r#"finish(-count);"#, -41.0),
        (r#"finish(+count);"#, 41.0),
        (r#"finish(count % 2);"#, 1.0),
    ] {
        assert_eq!(finished(source).await, Value::Number(expected), "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_bindings_compare_as_their_underlying_value() {
    for (source, expected) in [
        (r#"finish(text === "hello");"#, true),
        (r#"finish(text !== "hello");"#, false),
        (r#"finish(text == "hello");"#, true),
        (r#"finish(count === 41);"#, true),
        (r#"finish(count == 41);"#, true),
        (r#"finish(count > 40);"#, true),
        (r#"finish(text < "world");"#, true),
        (r#"finish(!text);"#, false),
    ] {
        assert_eq!(finished(source).await, Value::Bool(expected), "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn console_log_of_projected_bindings_prints_their_values() {
    // The exact shape that killed the FIG-1289 finale turn: a multi-argument
    // `console.log` over projected values, which lowers to `"" + a + " " + b`.
    let (outcome, printed) = execute(r#"console.log("row", text, count); finish(1);"#)
        .await
        .expect("a projected console.log should execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));
    assert_eq!(printed, vec!["row hello 41".to_string()]);
}
