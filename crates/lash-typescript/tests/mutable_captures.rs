//! ADR 0062 deviation register entry 5: mutable captures are refused on both
//! the read and the write path until durable lexical cells exist (FIG-3604).
//!
//! A closure copies what it captures when it is created, so a capture is exact
//! only while nothing assigns the binding after that copy. Only the write path
//! used to be refused: a closure that *read* a `let` reassigned after it was
//! created ran and silently answered the stale value where Node answers the
//! current one.
//!
//! The safe cases are pinned beside them, with Node's answers: the refusal is
//! about an assignment that can follow the copy, not about `let`.

use std::collections::BTreeSet;

use lash_typescript::DiagnosticCode;
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
                "unexpected mutable-capture ability",
            )),
        }
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("`{source}` should finish, got {other:?}"),
    }
}

fn refused(source: &str) -> lash_typescript::Diagnostic {
    let error = lash_typescript::testing::compile(source)
        .expect_err(&format!("`{source}` reads a stale capture and must reject"));
    assert_eq!(
        error.code,
        DiagnosticCode::MutableCaptureUnsupported,
        "`{source}`: {error}"
    );
    error
}

/// The read path: a closure created before an assignment it would miss.
///
/// Each of these compiled before FIG-3604 and answered its closure's stale
/// copy; the comment gives that answer and Node's.
#[test]
fn a_closure_reading_a_binding_assigned_after_it_was_created_rejects() {
    let cases = [
        // FIG-3599's `mutable-capture-read` row. Answered 1; Node 2.
        (
            "finish((() => { let x = 1; const f = () => x; x = 2; return f(); })());",
            "x",
        ),
        // The same at the top level of a cell. Answered 1; Node 2.
        ("let x = 1; const f = () => x; x = 2; finish(f());", "x"),
        // Every assignment form counts. Answered 1; Node 2, 5 and 7.
        ("let x = 1; const f = () => x; x++; finish(f());", "x"),
        ("let x = 1; const f = () => x; [x] = [5]; finish(f());", "x"),
        (
            "let x = 1; const f = () => x; for (x of [7]) { } finish(f());",
            "x",
        ),
        // A closure made by another closure copies from that one's copy.
        // Answered 1; Node 2.
        (
            "let x = 1; const outer = () => () => x; const g = outer(); x = 2; finish(g());",
            "x",
        ),
        // A hoisted declaration is created at the head of its block, before
        // an assignment that precedes it in the source. Answered 1; Node 2.
        (
            "let x = 1; x = 2; function f(): number { return x; } finish(f());",
            "x",
        ),
        // Answered undefined; Node 1.
        ("let x; function f() { return x; } x = 1; finish(f());", "x"),
        // A `var` exists before its declaration runs, so its initializer is an
        // assignment. Answered undefined; Node 1.
        ("function f() { return x; } var x = 1; finish(f());", "x"),
        // A later iteration's assignment follows an earlier iteration's
        // closure. Answered [1,2,3]; Node [3,3,3].
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { i = i + 1; fs.push(() => i); } finish(JSON.stringify(fs.map((f) => f())));",
            "i",
        ),
        // A loop's test runs on every iteration, so the body's assignment
        // follows the test's closure. Answered [0,1,2]; Node [2,2,2].
        (
            "const fs: any[] = []; let n = 0; while (fs.push(() => n) < 3) { n = n + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            "n",
        ),
        // A per-iteration binding assigned after the closure, in the same
        // iteration. Answered [0,2]; Node [1,3].
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { fs.push(() => i); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            "i",
        ),
        // Answered [1,2]; Node [9,9].
        (
            "const fs: any[] = []; for (let v of [1, 2]) { fs.push(() => v); v = 9; } finish(JSON.stringify(fs.map((f) => f())));",
            "v",
        ),
        // A top-level `var` is a property of the global object, so a
        // `globalThis` write assigns it. Answered 1; Node 2.
        (
            "var x = 1; const f = () => x; globalThis.x = 2; finish(f());",
            "x",
        ),
        // The dialect gives a top-level `let` the same session slot as its
        // `globalThis` property (register entry 19,
        // `global-object-aliases-lexical-bindings`), so these writes assign
        // the binding too, at the root or from inside a function, and the
        // closure's copy would disagree with the binding it copied.
        (
            "let x = 1; const f = () => x; globalThis.x = 2; finish(f());",
            "x",
        ),
        (
            "let x = 1; const f = () => x; function g() { globalThis.x = 2; } g(); finish(f());",
            "x",
        ),
        (
            "let x = 1; const f = () => x; delete globalThis.x; finish(f());",
            "x",
        ),
    ];
    for (source, name) in cases {
        let error = refused(source);
        assert!(
            error.message.contains(&format!("`{name}`")),
            "`{source}` must name `{name}`: {error}"
        );
        assert!(
            !error.suggestions.is_empty(),
            "`{source}` must carry a repair: {error}"
        );
    }
}

/// The write path: assigning to a captured binding from inside the closure.
#[test]
fn a_closure_assigning_a_captured_binding_rejects() {
    let cases = [
        "let n = 0; const f = () => { n = 5; }; f(); finish(n);",
        "let seen = 0; function f(): number { try { return 1; } finally { seen = 1; } } f(); finish(seen);",
    ];
    for source in cases {
        refused(source);
    }
}

/// What stays accepted, with Node's answers. None of these can see an
/// assignment after its closure's copy, so the copy is exact.
#[test]
fn captures_no_later_assignment_reaches_read_what_node_reads() {
    let cases = [
        // A `const`, and a `let` nothing reassigns.
        (
            "const k = 3; const f = () => k; finish(f());",
            Value::Number(3.0),
        ),
        (
            "let k = 3; const f = () => k; finish(f());",
            Value::Number(3.0),
        ),
        // Reassigned only before the closure exists.
        (
            "let x = 1; x = 2; const f = () => x; finish(f());",
            Value::Number(2.0),
        ),
        (
            "let x = 1; x++; const f = () => x; finish(f());",
            Value::Number(2.0),
        ),
        // A classic `for` binding is per-iteration, and its increment writes
        // the next iteration's copy.
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { fs.push(() => i); } finish(JSON.stringify(fs.map((f) => f())));",
            Value::String("[0,1,2]".into()),
        ),
        (
            "const fs: any[] = []; for (const v of [1, 2, 3]) { fs.push(() => v * 10); } finish(JSON.stringify(fs.map((f) => f())));",
            Value::String("[10,20,30]".into()),
        ),
        // A binding declared in a loop body is fresh each iteration, even one
        // assigned before its closure.
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { const snapshot = i; fs.push(() => snapshot); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            Value::String("[0,1,2]".into()),
        ),
        (
            "const fs: any[] = []; let i = 0; while (i < 3) { let v = i; v = v * 2; fs.push(() => v); i = i + 1; } finish(JSON.stringify(fs.map((f) => f())));",
            Value::String("[0,2,4]".into()),
        ),
        (
            "const fs: any[] = []; for (let i = 0; i < 3; i++) { let label = 'item'; label = label + i; fs.push(() => label); } finish(JSON.stringify(fs.map((f) => f())));",
            Value::String("[\"item0\",\"item1\",\"item2\"]".into()),
        ),
        // An accumulator read once the loop that assigns it is done.
        (
            "let total = 0; for (const n of [1, 2, 3]) { total = total + n; } finish(JSON.stringify([3, 6].map((v) => v / total)));",
            Value::String("[0.5,1]".into()),
        ),
        // Mutation *through* a captured reference is not an assignment to the
        // binding.
        (
            "const state = { n: 0 }; const bump = () => { state.n = state.n + 1; }; bump(); bump(); finish(state.n);",
            Value::Number(2.0),
        ),
        (
            "let o = { n: 1 }; const f = () => o.n; o.n = 5; finish(f());",
            Value::Number(5.0),
        ),
        (
            "function h(a: number) { return () => a; } finish(h(4)());",
            Value::Number(4.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A session global is judged inside the cell that captures it. A closure
/// never outlives its cell (`closure-boundary`), so an assignment in a later
/// cell cannot follow it; and a bare assignment to an earlier cell's binding
/// is refused, so the one way a cell assigns one is `globalThis.name`, which
/// the dialect aliases to the binding.
#[test]
fn a_session_global_capture_is_judged_in_the_cell_that_makes_it() {
    let globals = BTreeSet::from(["count".to_string()]);
    let cell = |source: &str| lash_typescript::parse_with_globals(source, &globals);

    cell("const f = () => count; finish(f());").expect("a capture nothing reassigns links");
    cell("globalThis.count = 2; const f = () => count; finish(f());")
        .expect("an assignment before the closure is not stale");
    let error = cell("const f = () => count; globalThis.count = 2; finish(f());")
        .expect_err("an assignment after the closure is stale");
    assert_eq!(error.code, DiagnosticCode::MutableCaptureUnsupported);
    assert!(error.message.contains("`count`"), "{error}");
}
