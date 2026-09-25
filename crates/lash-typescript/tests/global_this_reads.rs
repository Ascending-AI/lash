//! `globalThis.name` reads the session slot live, from anywhere (FIG-3620).
//!
//! A `globalThis.name` read used to lower to a load of `name` itself. At the
//! top level that was the session slot, but a function never captured it, so a
//! read inside any function or closure failed with `unknown name`. Where a
//! function did have something called `name` — a parameter or a local — the
//! read answered that instead of the global. A read lowered before a later
//! function's `globalThis` write was a static `undefined`. A top-level block
//! binding took the bare slot name, so a `globalThis` write inside the block
//! overwrote the block binding.
//!
//! Each case gives the answer before FIG-3620 and Node's, which is what it
//! answers now. The session oracle (`tests/differential/sessions`) holds the
//! same rows against Node itself, across a reload.

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
                "unexpected globalThis-read ability",
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

fn list(values: impl IntoIterator<Item = Value>) -> Value {
    Value::List(values.into_iter().collect::<Vec<_>>().into())
}

#[test]
fn a_function_or_closure_reads_the_session_slot() {
    let cases = [
        // Each was `unknown name` at run time before FIG-3620.
        (
            "var x = 1; function f() { return globalThis.x; } finish(f());",
            Value::Number(1.0),
        ),
        (
            "var x = 1; const f = () => globalThis.x; finish(f());",
            Value::Number(1.0),
        ),
        (
            "var x = 1; function f() { const g = () => globalThis.x; return g(); } finish(f());",
            Value::Number(1.0),
        ),
        (
            "var x = 1; const f = () => () => globalThis.x; finish(f()());",
            Value::Number(1.0),
        ),
        (
            "var k = 10; finish([1, 2].map((v) => v + globalThis.k));",
            list([Value::Number(11.0), Value::Number(12.0)]),
        ),
        (
            "var x = 1; function f() { return typeof globalThis.x; } finish(f());",
            Value::String("number".into()),
        ),
        (
            "var x = 1; function f() { return globalThis.x?.y; } finish(f());",
            Value::Undefined,
        ),
        (
            "var st = { n: 1 }; function f() { return globalThis.st.n; } finish(f());",
            Value::Number(1.0),
        ),
        // The read is the global's object, not a copy of it.
        (
            "var st = { n: 1 }; function f() { return globalThis.st; } finish(f() === st);",
            Value::Bool(true),
        ),
        (
            "var st = { n: 1 }; function f() { globalThis.st.n = 5; } f(); finish(st.n);",
            Value::Number(5.0),
        ),
        // A name nothing binds reads `undefined`, as an absent property does.
        (
            "function f() { return globalThis.nothere; } finish(f());",
            Value::Undefined,
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A read is live: it answers the value the global holds when the read runs,
/// so no copy a closure holds can go stale, and FIG-3604's mutable-capture
/// refusal has nothing to refuse. A bare-name capture still refuses.
#[test]
fn a_read_answers_the_value_when_it_runs() {
    let cases = [
        // Each was `unknown name` at run time before FIG-3620.
        (
            "var x = 1; const f = () => globalThis.x; globalThis.x = 2; finish(f());",
            Value::Number(2.0),
        ),
        (
            "var x = 1; function f() { return globalThis.x; } function w() { globalThis.x = 3; } w(); finish(f());",
            Value::Number(3.0),
        ),
        // Answered undefined, lowered before the write was seen; Node 5.
        (
            "function g() { return globalThis.y; } function s() { globalThis.y = 5; } s(); finish(g());",
            Value::Number(5.0),
        ),
        // Was `unknown name`; Node [undefined, 5].
        (
            "function g() { return globalThis.y; } const before = g(); globalThis.y = 5; finish([before, g()]);",
            list([Value::Undefined, Value::Number(5.0)]),
        ),
        // A read before the declaration runs finds no global yet.
        (
            "function f() { return globalThis.x; } const a = f(); var x = 1; finish([a, f()]);",
            list([Value::Undefined, Value::Number(1.0)]),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
    // A bare-name read of the same global is live too (FIG-3707): the closure
    // reads the session slot the write assigned.
    assert_eq!(
        finished("var x = 1; const f = () => x; globalThis.x = 2; finish(f());"),
        Value::Number(2.0),
    );
}

#[test]
fn a_binding_of_the_same_name_does_not_answer_a_global_read() {
    let cases = [
        // Answered 5; Node undefined.
        (
            "function f(x) { return globalThis.x; } finish(f(5));",
            Value::Undefined,
        ),
        // Answered ["param", "param"].
        (
            "var x = 'global'; function f(x) { return [x, globalThis.x]; } finish(f('param'));",
            list([
                Value::String("param".into()),
                Value::String("global".into()),
            ]),
        ),
        // Answered [9, 9].
        (
            "var x = 1; function f() { const x = 9; return [x, globalThis.x]; } finish(f());",
            list([Value::Number(9.0), Value::Number(1.0)]),
        ),
        // Answered 9; Node undefined.
        (
            "function f() { var x = 9; return globalThis.x; } finish(f());",
            Value::Undefined,
        ),
        // A top-level block binding is not a global. Answered 1; Node
        // undefined, at the top level and from a closure alike.
        ("{ let x = 1; finish(globalThis.x); }", Value::Undefined),
        (
            "{ let x = 1; const f = () => globalThis.x; finish(f()); }",
            Value::Undefined,
        ),
        // A write in the block makes a separate global. Answered [2, 2].
        (
            "{ let x = 1; globalThis.x = 2; finish([x, globalThis.x]); }",
            list([Value::Number(1.0), Value::Number(2.0)]),
        ),
        (
            "var x = 1; { let x = 2; finish([x, globalThis.x]); }",
            list([Value::Number(2.0), Value::Number(1.0)]),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}

/// A global a function's `globalThis` write creates does not exist until the
/// write runs. It used to be pre-assigned `undefined` at the top of the cell,
/// so it existed (and survived into the session) whether or not the write ran.
#[test]
fn a_function_written_global_exists_only_once_written() {
    let cases = [
        // Answered true.
        (
            "function s() { globalThis.y = 5; } finish('y' in globalThis);",
            Value::Bool(false),
        ),
        (
            "function s() { globalThis.y = 5; } finish(typeof y);",
            Value::String("undefined".into()),
        ),
        (
            "function s() { globalThis.y = 5; } s(); finish(['y' in globalThis, typeof y, y]);",
            list([
                Value::Bool(true),
                Value::String("number".into()),
                Value::Number(5.0),
            ]),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
    let program = lash_typescript::testing::compile("function s() { globalThis.y = 5; }")
        .expect("an unrun global write compiles");
    let mut state = State::new();
    futures::executor::block_on(lashlang::execute(&program, &mut state, &Host))
        .expect("the cell runs");
    assert!(
        state.globals().get("y").is_none(),
        "an unrun write leaves no session global: {:?}",
        state.globals()
    );
}

/// An earlier cell's global, read from a function in a later cell.
#[test]
fn a_function_reads_an_earlier_cells_global() {
    let mut state = State::new();
    let first = lash_typescript::testing::compile("var count = 4;").expect("first cell compiles");
    futures::executor::block_on(lashlang::execute(&first, &mut state, &Host))
        .expect("first cell runs");
    let globals = BTreeSet::from(["count".to_string()]);
    let second = lash_typescript::parse_with_globals(
        "function read() { return globalThis.count; } finish([read(), (() => globalThis.count)()]);",
        &globals,
    )
    .expect("second cell lowers");
    let artifact = lashlang::ModuleArtifact::from_program(second).expect("second cell artifact");
    let second = lashlang::compile(&artifact, lashlang::Entry::Main, None).expect("compiles");
    match futures::executor::block_on(lashlang::execute(&second, &mut state, &Host))
        .expect("second cell runs")
    {
        ExecutionOutcome::Finished(value) => {
            assert_eq!(value, list([Value::Number(4.0), Value::Number(4.0)]));
        }
        other => panic!("the second cell should finish, got {other:?}"),
    }
}

/// A process body runs apart from the cell that starts it and sees only the
/// values it started with, so it has no live view of session state. The read
/// used to fail to link with `unknown name`; the write, presence check and
/// delete linked and silently acted on the process's own root instead.
#[test]
fn a_process_body_refuses_every_global_this_form() {
    for body in [
        "globalThis.budget",
        "globalThis.budget.n",
        "{ globalThis.budget = 1; return 1; }",
        "'budget' in globalThis",
        "delete globalThis.budget",
    ] {
        let source = format!("var budget = {{ n: 40 }}; const spend = async () => {body};");
        let error = lash_typescript::parse(&source)
            .expect_err(&format!("`{source}` must refuse in a process body"));
        assert_eq!(
            error.code,
            DiagnosticCode::NonLiftableCapture,
            "{source}: {error}"
        );
        assert!(error.message.contains("`budget`"), "{source}: {error}");
        assert!(!error.suggestions.is_empty(), "{source}: {error}");
    }
}

/// A function declaration is hoisted ahead of the `globalThis` write that
/// creates a session slot, and reads the slot by name when it runs after the
/// write (FIG-3707's generated sessions, seed 390). Each was refused as
/// `TS_UNKNOWN_BINDING` before: the hoisted body lowered before the write
/// declared the slot.
#[test]
fn a_hoisted_function_reads_a_slot_a_later_global_this_write_creates() {
    let cases = [
        (
            "globalThis.shared = 'b'; function read() { return shared + '!'; } finish(read());",
            Value::String("b!".into()),
        ),
        (
            "function read() { return typeof shared; } const before = read(); globalThis.shared = 1; finish([before, read(), shared]);",
            list([
                Value::String("undefined".into()),
                Value::String("number".into()),
                Value::Number(1.0),
            ]),
        ),
        (
            "function bump() { globalThis.hits = (globalThis.hits ?? 0) + 1; return hits; } bump(); finish(bump());",
            Value::Number(2.0),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(finished(source), expected, "{source}");
    }
}
