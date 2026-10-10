//! Asynchronous source runs as kernel tasks with the interleaving Node
//! gives.
//!
//! `witness/async/recorded.json` holds what Node did with each case of
//! `cases.mjs` under a scripted order of arrivals (`record.mjs` writes it).
//! The laws here lower the same cases, run them on the kernel machine under
//! the same script and compare, stretch by stretch: the tool calls made,
//! the lines logged, which carry the values, the errors and the order of
//! writes to shared state, and how the run ended.

use serde::Deserialize;

use super::machine::{Epoch, Recorded, run};
use super::{lower, lower_with_effects, main_text_with_effects};
use crate::DiagnosticCode;

#[derive(Deserialize)]
struct Witness {
    name: String,
    deviation: Option<String>,
    source: String,
    deliveries: Vec<Vec<String>>,
    #[serde(flatten)]
    node: Recorded,
}

#[derive(Deserialize)]
struct Witnesses {
    cases: Vec<Witness>,
}

fn witnesses() -> Vec<Witness> {
    let recorded: Witnesses =
        serde_json::from_str(include_str!("../../witness/async/recorded.json"))
            .expect("recorded.json is what record.mjs writes");
    recorded.cases
}

fn witness(name: &str) -> Witness {
    witnesses()
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no witness `{name}`; run witness/async/record.mjs"))
}

fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_string()).collect()
}

/// Every witness without a registered deviation runs on the kernel as it
/// ran in Node: the same requests, lines and end in every stretch.
#[test]
fn async_cells_interleave_as_node_does() {
    let cases = witnesses();
    assert!(cases.len() >= 18, "{} witnesses", cases.len());
    for case in cases.iter().filter(|case| case.deviation.is_none()) {
        let kernel = run(&case.source, &case.deliveries);
        assert_eq!(kernel, case.node, "{}\n{}", case.name, case.source);
    }
}

/// The resume case: three tasks parked on one tool call each, delivered in
/// two different orders. Each order gives what Node gives for it, and the
/// two differ in the order of writes to the counter the tasks share.
#[test]
fn three_parked_tasks_resume_in_arrival_order() {
    let one = witness("fan_out_shares_a_counter");
    let other = witness("fan_out_other_arrival_order");
    assert_eq!(one.source, other.source);
    let first = run(&one.source, &one.deliveries);
    let second = run(&other.source, &other.deliveries);
    assert_eq!(
        first.epochs[0].asked,
        lines(&["echo:a1", "echo:b1", "echo:c1"])
    );
    assert_eq!(first, one.node);
    assert_eq!(second, other.node);
    assert_eq!(first.epochs[1].logged, lines(&["b first b1 1"]));
    assert_eq!(
        second.epochs[1].logged,
        lines(&["c first c1 1", "b first b1 2", "a first a1 3"])
    );
}

/// `K-TASK-008`: an aggregate registered before a direct waiter wakes
/// first, so their continuations interleave as Node's promise reactions do.
#[test]
fn single_and_list_waiters_interleave_as_node_does() {
    let case = witness("single_and_list_waiters_on_one_promise");
    assert!(case.deviation.is_none());
    assert_eq!(case.node.lines(), ["single 1", "all", "single 2"]);
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// `TS_RUN_END_STRICT`: a cell that ends while a promise's task is still
/// running ends in the kernel's typed error (`K-TASK-018`); Node would
/// keep the process alive until the task ended.
#[test]
fn a_cell_that_ends_with_a_running_task_is_an_error() {
    let recorded = run("const pending = echo(\"a\"); console.log(\"end\");", &[]);
    assert_eq!(recorded.lines(), ["end"]);
    assert_eq!(
        recorded.end,
        "tasks outstanding: 1 unfinished, 0 unobserved"
    );
}

/// `TS_UNHANDLED_REJECTION`: a promise that rejected and that nothing
/// awaited, chained from or aggregated by the cell's end makes the end the
/// kernel's typed error. One handled before the end is no error, however
/// late the handler comes: here a tool call's arrival stands between the
/// rejection and the `catch`, where Node would already have ended the
/// process.
#[test]
fn a_rejection_is_unhandled_only_at_the_cells_end() {
    let dropped = run(
        "const failing = async () => { throw \"lost\"; }; const p = failing(); console.log(\"end\");",
        &[],
    );
    assert_eq!(dropped.lines(), ["end"]);
    assert_eq!(dropped.end, "tasks outstanding: 0 unfinished, 1 unobserved");

    let late = run(
        "const failing = async () => { throw \"late\"; };\n\
         const p = failing();\n\
         await echo(\"a\");\n\
         try { await p; } catch (error) { console.log(`caught ${error}`); }",
        &[],
    );
    assert_eq!(late.lines(), ["caught late"]);
    assert_eq!(late.end, "ok");
}

/// `TS_PROMISE_RACE_EMPTY`: `Promise.race([])` rejects with the kernel's
/// `empty_join` (`K-TASK-013`); in JavaScript it never settles.
#[test]
fn a_race_of_nothing_rejects() {
    let recorded = run(
        "try { await Promise.race([]); } catch (error) { console.log(`${error}`); }",
        &[],
    );
    assert_eq!(recorded.lines().len(), 1);
    assert!(
        recorded.lines()[0].starts_with("empty_join: "),
        "{recorded:?}"
    );
    assert_eq!(recorded.end, "ok");
}

/// `TS_AWAIT_THENABLE`: an object with a `then` method is not a promise.
/// Awaiting it gives the object, one turn of the queue later.
#[test]
fn a_thenable_is_a_plain_value() {
    let recorded = run(
        "const thenable = { then: (resolve) => resolve(1) };\n\
         const value = await thenable;\n\
         console.log(typeof value, value === thenable);",
        &[],
    );
    assert_eq!(recorded.lines(), ["object true"]);
    assert_eq!(recorded.end, "ok");
}

/// An error that leaves the cell uncaught ends the run in it.
#[test]
fn an_awaited_rejection_nothing_catches_ends_the_run() {
    let recorded = run("await boom(\"x\");", &[]);
    assert_eq!(
        recorded.epochs,
        [
            Epoch {
                delivered: Vec::new(),
                asked: lines(&["boom:x"]),
                logged: Vec::new(),
            },
            Epoch {
                delivered: lines(&["boom:x"]),
                asked: Vec::new(),
                logged: Vec::new(),
            },
        ]
    );
    assert_eq!(recorded.end, "error boom: x");
}

/// `await sleep(ms)` is one `sleep` in the awaiting task.
#[test]
fn an_awaited_sleep_is_one_sleep() {
    assert_eq!(
        main_text_with_effects("await sleep(5);"),
        "do sleep 5.0\ndo yield"
    );
}

/// A tool call whose promise nothing can read is refused where the front
/// end can see it; at run time it would be the strict error at the cell's
/// end.
#[test]
fn a_discarded_tool_call_is_refused() {
    for source in [
        "echo(1);",
        "void echo(1);",
        "async function f() { echo(1); }",
    ] {
        let error = lower_with_effects(source).expect_err(source);
        assert_eq!(error.code, DiagnosticCode::UnawaitedTool, "{source}");
    }
    assert!(lower_with_effects("const p = echo(1); await p;").is_ok());
    // A name the cell binds is its own function, not the host's tool.
    assert!(lower("const echo = (x) => x; echo(1);").is_ok());
}

/// A tool is called with the arguments its signature takes.
#[test]
fn a_tool_call_with_the_wrong_number_of_arguments_is_refused() {
    let error = lower_with_effects("await echo();").expect_err("too few");
    assert_eq!(error.code, DiagnosticCode::UnsupportedExpression);
    assert!(lower_with_effects("await echo(1, 2);").is_err());
}

/// A tool's argument crosses as plain data: a function the program made
/// or a built-in, at the top or inside an array, an object or a map, is
/// refused with `not_data` before the tool is asked.
#[test]
fn a_function_in_a_tool_argument_is_not_data() {
    for argument in [
        "() => 1",
        "Math",
        "[1, [2, () => 1]]",
        "{ a: 1, b: { f: Math.max } }",
        "new Map([['k', [Math.abs]]])",
    ] {
        assert_eq!(
            super::machine::end(&format!("const value = {argument}; await finish(value);")),
            super::machine::Ended::Raised("not_data".to_string()),
            "{argument}"
        );
    }
}

/// `new Promise(executor)` stays refused: a promise is settled by its
/// task's end, and nothing else can end a task.
#[test]
fn the_promise_constructor_is_refused() {
    let error = lower("new Promise((resolve) => resolve(1));").expect_err("no constructor");
    assert_eq!(error.code, DiagnosticCode::NewUnsupported);
}

/// `instanceof Promise` calls its unary predicate with the tested value,
/// including after the kernel document has been printed as TypeScript.
#[test]
fn promise_instanceof_preserves_its_operand_through_printing() {
    let source = "const f = async () => {}; const p = f(); \
                  console.log(p instanceof Promise, 1 instanceof Promise);";
    let original = lower(source).expect("promise predicates lower");
    let printed = crate::print(&original.document).expect("the document prints");
    let environment = lash_kernel_dialect::Environment {
        library: super::library(),
        effects: &std::collections::BTreeMap::new(),
        controls: &std::collections::BTreeMap::new(),
        tool_roots: &std::collections::BTreeSet::new(),
        bindings: &std::collections::BTreeSet::new(),
        functions: &std::collections::BTreeMap::new(),
    };
    let re_lowered =
        crate::lower_kernel_text(&printed, &environment).expect("printed promise predicates lower");
    // The same document runs the same.
    assert_eq!(original.document, re_lowered.document);
    let recorded = run(source, &[]);
    assert_eq!(recorded.lines(), ["true false"]);
    assert_eq!(recorded.end, "ok");
}
