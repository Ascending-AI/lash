//! ECMA coercion of a *projected* host binding.
//!
//! A session global supplied by the host is a `Value::Projected`: a lazy handle
//! the runtime reads through instead of a materialized tree. Every non-path
//! operation is supposed to strip that wrapper and evaluate the value behind
//! it — that is what `Binary` (the Lashlang arithmetic opcode) does by
//! materializing a projected operand synchronously.
//!
//! The TypeScript opcodes `JavaScriptUnary`/`JavaScriptBinary` did not. A
//! projected operand fell straight into the scalar ECMA coercions, whose
//! `Value::Projected` arm was a `debug_assert` plus a fallback: debug builds
//! panicked out of the turn, and release builds silently carried
//! `"[object Object]"` (or `NaN`, or `false`) for a value whose ECMA result is
//! the projected string, number, or comparison. `console.log(item.kind, item.id)`
//! over a projected history is how a workbench turn died (FIG-1446); at the
//! time that call lowered to `"" + a + " " + b`, so it reached the scalar
//! coercions directly. It no longer does — the arguments now go to the
//! observation renderer untouched — but the same projected operands still reach
//! those coercions through `+`, template literals and `String()`, which is what
//! this suite pins.
//!
//! The rule pinned here: a projected binding coerces exactly as the value
//! behind it does, in both build profiles, whether it reaches the coercion as
//! an operand of a TypeScript operator, as an argument to a stdlib call, or as
//! an element of a container the guest built from it.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ProjectedBindings, ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse,
    ProjectedValue, RuntimeError, State, Value,
};

#[derive(Default)]
struct Host {
    printed: Mutex<Vec<String>>,
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(value) => {
                self.printed
                    .lock()
                    .expect("print log is not poisoned")
                    .push(match value {
                        Value::String(value) => value.to_string(),
                        other => format!("{other:?}"),
                    });
                Ok(AbilityOutcome::Value(Value::Null))
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
    let program = lashlang::testing::harness::try_compile_program(&program)
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

/// A descriptor whose `Materialize` read counts its calls. Descriptor reads are
/// synchronous and pure over already-available state, so "pending" is
/// unexpressible; what these tests still pin is that the coercion paths read
/// the projected value on demand, and that identity comparisons never read.
struct CountingDescriptor {
    value: Value,
    materialize_calls: Arc<AtomicUsize>,
}

impl ProjectedHostDescriptor for CountingDescriptor {
    fn type_name(&self) -> &str {
        "CountingDescriptor"
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        match request {
            ProjectedReadRequest::Materialize => {
                self.materialize_calls.fetch_add(1, Ordering::Relaxed);
                Some(ProjectedReadResponse::Value(self.value.clone()))
            }
            _ => None,
        }
    }
}

struct CountingHost {
    projected: ProjectedValue,
}

impl ExecutionHost for CountingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unsupported counting projected-coercion ability",
            )),
        }
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        let mut bindings = ProjectedBindings::new();
        bindings.insert("pending", self.projected.clone());
        bindings
    }
}

async fn execute_counting(
    source: &str,
    value: Value,
    materialize_calls: Arc<AtomicUsize>,
) -> Result<ExecutionOutcome, RuntimeError> {
    let globals = BTreeSet::from(["pending".to_string()]);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let program = lashlang::testing::harness::try_compile_program(&program)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let projected = ProjectedValue::custom(
        "pending",
        Arc::new(CountingDescriptor {
            value,
            materialize_calls,
        }),
    );
    let mut runtime_state = State::new();
    lashlang::execute(&program, &mut runtime_state, &CountingHost { projected }).await
}

async fn run_counting_projection(
    source: &'static str,
    value: Value,
) -> (Result<ExecutionOutcome, RuntimeError>, usize) {
    let materialize_calls = Arc::new(AtomicUsize::new(0));
    let result = execute_counting(source, value, materialize_calls.clone()).await;
    (result, materialize_calls.load(Ordering::Relaxed))
}

async fn assert_counting_projection_completes(source: &'static str, value: Value, expected: Value) {
    let (result, materialize_calls) = run_counting_projection(source, value).await;
    assert!(
        materialize_calls > 0,
        "`{source}` must observe a materializing read"
    );
    assert_eq!(
        result.unwrap_or_else(|error| panic!("`{source}` should execute: {error}")),
        ExecutionOutcome::Finished(expected),
        "{source}"
    );
}

async fn assert_counting_projection_is_not_read(source: &'static str, expected: Value) {
    let (result, materialize_calls) =
        run_counting_projection(source, Value::String("must stay unread".into())).await;
    assert_eq!(
        materialize_calls, 0,
        "`{source}` must not read an object member that equality or truthiness does not coerce"
    );
    assert_eq!(
        result.unwrap_or_else(|error| panic!("`{source}` should execute: {error}")),
        ExecutionOutcome::Finished(expected),
        "{source}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn projected_operands_redispatch_through_the_synchronous_read() {
    for (source, value, expected) in [
        (
            r#"finish(pending + "!");"#,
            Value::String("hello".into()),
            Value::String("hello!".into()),
        ),
        (
            r#"finish(+pending);"#,
            Value::Number(41.0),
            Value::Number(41.0),
        ),
        (
            r#"finish([pending] + "!");"#,
            Value::String("hello".into()),
            Value::String("hello!".into()),
        ),
        (
            r#"finish(+[pending]);"#,
            Value::Number(41.0),
            Value::Number(41.0),
        ),
    ] {
        assert_counting_projection_completes(source, value, expected).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_object_comparisons_preserve_identity_without_reading_members() {
    for (source, expected) in [
        (r#"const a = [pending]; finish(a == a);"#, true),
        (r#"const a = [pending]; finish(a != a);"#, false),
        (
            r#"const a = [pending]; const b = [pending]; finish(a == b);"#,
            false,
        ),
        (
            r#"const a = [pending]; const b = [pending]; finish(a != b);"#,
            true,
        ),
        (r#"const a = [pending]; finish(a == null);"#, false),
        (r#"const a = [pending]; finish(a != null);"#, true),
        (r#"const a = [pending]; finish(a == undefined);"#, false),
        (r#"const a = [pending]; finish(a != undefined);"#, true),
        (r#"const a = [pending]; finish(a === a);"#, true),
        (r#"const a = [pending]; finish(a !== a);"#, false),
        (r#"const a = [pending]; finish(!a);"#, false),
    ] {
        assert_counting_projection_is_not_read(source, Value::Bool(expected)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_date_addition_concatenates_in_both_operand_positions() {
    // FIG-3704: a Date's default ToPrimitive hint is string, so `+` with a
    // materialized projection concatenates the deterministic UTC DateString.
    const DATE: &str = "Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)";
    for (source, expected) in [
        (r#"finish(new Date(0) + pending);"#, format!("{DATE}hello")),
        (r#"finish(pending + new Date(0));"#, format!("hello{DATE}")),
    ] {
        assert_counting_projection_completes(
            source,
            Value::String("hello".into()),
            Value::String(expected.into()),
        )
        .await;
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
    // `console.log` over projected values. The lowering has since changed — the
    // observation renderer receives the values instead of a concatenation — so
    // this pins the outcome, that a projected binding still prints the value
    // behind it, rather than the route it takes.
    let (outcome, printed) = execute(r#"console.log("row", text, count); finish(1);"#)
        .await
        .expect("a projected console.log should execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));
    assert_eq!(printed, vec!["row hello 41".to_string()]);
}
