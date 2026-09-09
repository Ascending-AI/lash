//! `console.*` renders its arguments for an RLM observation.
//!
//! The prompt tells the model to inspect values with `console.log`, so the text
//! that reaches the observation has to describe the value. JavaScript's own
//! string coercion answers `"[object Object]"` for every plain object and
//! comma-joins arrays, which is exactly the case a cell reaches for. Objects and
//! arrays therefore render as JSON — the same shape Lashlang's `print` shows —
//! while every other value keeps JavaScript's coercion.

use lashlang::{AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, State, Value};

#[derive(Default)]
struct PrintHost(std::sync::Mutex<Vec<String>>);

impl ExecutionHost for PrintHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                let Value::String(text) = value else {
                    return Err(ExecutionHostError::new(format!(
                        "console rendering should reach the host as text, got {value:?}"
                    )));
                };
                self.0.lock().expect("print lock").push(text.to_string());
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported test ability")),
        }
    }
}

/// Runs `source` and returns every printed observation line.
fn printed(source: &str) -> Vec<String> {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    let host = PrintHost::default();
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("TypeScript should execute");
    let lines = host.0.lock().expect("print lock");
    lines.clone()
}

#[derive(Default)]
struct RawPrintHost(std::sync::Mutex<Vec<Value>>);

impl ExecutionHost for RawPrintHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Print(value) => {
                self.0.lock().expect("print lock").push(value);
                Ok(AbilityResult::Unit)
            }
            _ => Err(ExecutionHostError::new("unsupported test ability")),
        }
    }
}

/// Runs `source` and returns the values `print` received, unrendered.
fn printed_values(source: &str) -> Vec<Value> {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    let host = RawPrintHost::default();
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect("TypeScript should execute");
    let values = host.0.lock().expect("print lock");
    values.clone()
}

fn printed_line(source: &str) -> String {
    let lines = printed(source);
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one observation: {lines:?}"
    );
    lines.into_iter().next().expect("one observation")
}

#[test]
fn records_render_as_json() {
    assert_eq!(
        printed_line("console.log({ a: 1, b: [2, 3] });"),
        r#"{"a":1,"b":[2,3]}"#
    );
}

#[test]
fn strings_pass_through_and_join_arguments_with_one_space() {
    assert_eq!(
        printed_line(r#"console.log("x", { a: 1 });"#),
        r#"x {"a":1}"#
    );
    assert_eq!(printed_line(r#"console.log("a", "b", "c");"#), "a b c");
}

#[test]
fn arrays_of_records_render_as_json() {
    assert_eq!(
        printed_line("console.log([{ id: 1 }, { id: 2 }]);"),
        r#"[{"id":1},{"id":2}]"#
    );
}

#[test]
fn scalars_keep_javascript_coercion() {
    assert_eq!(printed_line("console.log(null);"), "null");
    assert_eq!(printed_line("console.log(undefined);"), "undefined");
    assert_eq!(printed_line("console.log(true, false);"), "true false");
    assert_eq!(printed_line("console.log(1, 2.5, -0);"), "1 2.5 0");
    assert_eq!(printed_line("console.log(NaN, Infinity);"), "NaN Infinity");
    assert_eq!(printed_line("console.log(1e21);"), "1e+21");
}

#[test]
fn nested_maps_and_sets_render_as_their_json_form() {
    // A Map/Set has no JSON body of its own, exactly as in Node: nested it is
    // `{}`, and standalone it keeps JavaScript's `[object Map]` coercion, which
    // at least names the type the cell has to convert.
    assert_eq!(
        printed_line("console.log({ m: new Map([[\"k\", 1]]), s: new Set([1]) });"),
        r#"{"m":"[object Map]","s":"[object Set]"}"#
    );
    assert_eq!(printed_line("console.log(new Map());"), "[object Map]");
    assert_eq!(printed_line("console.log(new Set());"), "[object Set]");
}

#[test]
fn objects_without_a_json_body_keep_their_javascript_string() {
    // A `Date`, `RegExp` or `Error` inside a record still describes itself; the
    // renderer never drops the rest of the record to accommodate it.
    assert_eq!(
        printed_line("console.log({ pattern: /ab+c/gi, failure: new Error(\"boom\"), n: 1 });"),
        r#"{"pattern":"/ab+c/gi","failure":"Error: boom","n":1}"#
    );
}

#[test]
fn undefined_follows_json_container_rules() {
    assert_eq!(
        printed_line("console.log({ a: undefined, b: 1 });"),
        r#"{"b":1}"#
    );
    assert_eq!(printed_line("console.log([undefined, 1]);"), "[null,1]");
}

#[test]
fn every_console_method_renders_the_same_way() {
    for method in ["log", "info", "warn", "error", "debug"] {
        assert_eq!(
            printed_line(&format!("console.{method}({{ a: 1 }});")),
            r#"{"a":1}"#,
            "console.{method} should render like console.log"
        );
    }
}

/// A cell cannot build a cyclic object in the first place — the heap refuses
/// the assignment that would close the cycle — so the renderer's `[Circular]`
/// guard is defence in depth, matching the one `JSON.stringify` already keeps.
/// Pinned so the refusal is not mistaken for a rendering defect.
#[test]
fn cyclic_objects_are_refused_before_they_can_be_printed() {
    let program = lash_typescript::compile("const a: any = {}; a.self = a; console.log(a);")
        .expect("compile");
    let host = PrintHost::default();
    let error = futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
        .expect_err("a cyclic object never reaches the observation");
    assert!(
        format!("{error:?}").contains("Cyclic"),
        "expected the heap's cyclic refusal, got {error:?}"
    );
}

#[test]
fn console_with_no_arguments_prints_an_empty_observation() {
    assert_eq!(printed_line("console.log();"), "");
}

#[test]
fn string_concatenation_stays_javascript_faithful() {
    // The fix owns the observation seam only; `"" + obj` and template literals
    // keep ECMAScript's answer.
    assert_eq!(
        printed_line(r#"console.log("" + { a: 1 });"#),
        "[object Object]"
    );
    assert_eq!(
        printed_line("console.log(`${{ a: 1 }}`);"),
        "[object Object]"
    );
    assert_eq!(
        printed_line("console.log(String({ a: 1 }));"),
        "[object Object]"
    );
    assert_eq!(printed_line("console.log([1, 2].join(\",\"));"), "1,2");
}

#[test]
fn a_shadowed_console_binding_still_calls_the_binding() {
    // `console` is only the host inspector while nothing in scope shadows it:
    // a shadowing binding keeps calling its own function, which hands `print`
    // the value itself rather than observation text.
    assert!(matches!(
        printed_values(
            "const console = { log: (value: unknown) => { print(value); } }; console.log({ a: 1 });",
        )
        .as_slice(),
        [Value::Record(_)]
    ));
}

/// `print` hands the host the value itself, which the RLM renders with the same
/// compact JSON projector. Pinned so the two inspect paths cannot drift apart.
#[test]
fn print_hands_the_host_the_value_itself() {
    assert!(matches!(
        printed_values("print({ a: 1, b: [2, 3] });").as_slice(),
        [Value::Record(_)]
    ));
}
