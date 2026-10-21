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
use super::{lower, lower_with_effects};
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
        lines(&["host.echo:a1", "host.echo:b1", "host.echo:c1"])
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
    let recorded = run(
        "const pending = host.echo(\"a\"); console.log(\"end\");",
        &[],
    );
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
         await host.echo(\"a\");\n\
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
    let recorded = run("await host.boom(\"x\");", &[]);
    assert_eq!(
        recorded.epochs,
        [
            Epoch {
                delivered: Vec::new(),
                asked: lines(&["host.boom:x"]),
                logged: Vec::new(),
            },
            Epoch {
                delivered: lines(&["host.boom:x"]),
                asked: Vec::new(),
                logged: Vec::new(),
            },
        ]
    );
    assert_eq!(recorded.end, "error boom: x");
}

/// A tool call whose promise nothing can read is refused where the front
/// end can see it; at run time it would be the strict error at the cell's
/// end.
#[test]
fn a_discarded_tool_call_is_refused() {
    for source in [
        "host.echo(1);",
        "void host.echo(1);",
        "async function f() { host.echo(1); }",
    ] {
        let error = lower_with_effects(source).expect_err(source);
        assert_eq!(error.code, DiagnosticCode::UnawaitedTool, "{source}");
    }
    assert!(lower_with_effects("const p = host.echo(1); await p;").is_ok());
    // A name the cell binds is its own function, not the host's tool.
    assert!(lower("const echo = (x) => x; echo(1);").is_ok());
}

/// A tool is called with the arguments its signature takes.
#[test]
fn a_tool_call_with_the_wrong_number_of_arguments_is_refused() {
    let error = lower_with_effects("await host.echo();").expect_err("too few");
    assert_eq!(error.code, DiagnosticCode::UnsupportedExpression);
    assert!(lower_with_effects("await host.echo(1, 2);").is_err());
}

/// FN-CROSS: guest functions cross as frozen function values, including
/// inside arrays. Builtins still refuse with `not_data` before dispatch.
#[test]
fn tool_arguments_freeze_guest_functions_but_refuse_builtins() {
    use lash_kernel_doc::{Datum, Float};

    use super::machine::{Ended, end};

    let direct = end("const value = () => 1; await control.finish(value);");
    let Ended::Finished(Datum::FunctionValue(function)) = direct else {
        panic!("a guest function crosses as a function value: {direct:?}");
    };
    assert_eq!(
        end("const value = [1, [2, () => 1]]; await control.finish(value);"),
        Ended::Finished(Datum::List(vec![
            Datum::Float(Float::new(1.0)),
            Datum::List(vec![
                Datum::Float(Float::new(2.0)),
                Datum::FunctionValue(function)
            ]),
        ]))
    );
    for argument in [
        "Math",
        "{ a: 1, b: { f: Math.max } }",
        "new Map([['k', [Math.abs]]])",
    ] {
        assert_eq!(
            end(&format!(
                "const value = {argument}; await control.finish(value);"
            )),
            Ended::Raised("not_data".to_string()),
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

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn promise_aggregates_follow_reaction_order() {
    let case = witness("promise_aggregates_follow_reaction_order");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn for_initializer_closure_keeps_its_binding() {
    let case = witness("for_initializer_closure_keeps_its_binding");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn computed_rest_excludes_canonical_keys() {
    let case = witness("computed_rest_excludes_canonical_keys");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn array_from_steps_growing_collections() {
    let case = witness("array_from_steps_growing_collections");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn destructuring_steps_live_iterators() {
    let case = witness("destructuring_steps_live_iterators");
    assert_eq!(
        run(&case.source, &case.deliveries),
        case.node,
        "{}",
        super::main_text(&case.source)
    );
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn error_conversion_reads_current_properties() {
    let case = witness("error_conversion_reads_current_properties");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-5905: the source computes the recorded Node result.
#[test]
fn null_base_assignment_precedes_key_conversion() {
    let case = witness("null_base_assignment_precedes_key_conversion");
    assert_eq!(run(&case.source, &case.deliveries), case.node);
}

/// FIG-6524: only free callees are tool candidates, at their lexical scope.
#[test]
fn tool_discovery_resolves_lexical_values_before_namespaces() {
    let bindings =
        std::collections::BTreeSet::from([lash_vocabulary::legacy_names::Name::new("previous")]);
    let calls = crate::Parser::default()
        .tool_calls(
            r#"
        const text = "phantom.call()";
        function order(items) { items.sort((a, b) => a.key - b.key); }
        { const items = []; items.sort(); }
        previous.sort();
        [{key: 2}, {key: 1}].sort((a, b) => a.key - b.key);
        console.log(text);
        // phantom.comment()
        await items.sort({});
        await other.fetch({});
        "#,
            &lash_kernel_dialect::Environment::new(
                super::library(),
                &crate::tests::linked::namespaces(&Default::default(), &Default::default())
                    .expect("the test's effects are operations"),
            ),
            lash_kernel_dialect::CellEnvironment {
                bindings: &bindings,
                functions: &Default::default(),
            },
        )
        .expect("discover calls in their scopes");
    assert_eq!(
        calls,
        std::collections::BTreeSet::from(["items.sort".into(), "other.fetch".into()])
    );
}

/// FIG-6556: discovery is lexical even when lowering refuses earlier code;
/// bare unknown names never become deferred tool authority.
#[test]
fn tool_discovery_survives_refusals_and_ignores_bare_calls() {
    for source in [
        "Math.random(); workflows_create(); await tools.lookup({ nested: other.fetch({}) });",
        "(1).toLocaleString(); workflows_create(); await tools.lookup({ nested: other.fetch({}) });",
    ] {
        let calls = crate::Parser::default()
            .tool_calls(
                source,
                &lash_kernel_dialect::Environment::new(
                    super::library(),
                    &crate::tests::linked::namespaces(&Default::default(), &Default::default())
                        .expect("the test's effects are operations"),
                ),
                lash_kernel_dialect::CellEnvironment::first(),
            )
            .expect("discovery does not lower unrelated refusals");
        assert_eq!(
            calls,
            std::collections::BTreeSet::from(["tools.lookup".into(), "other.fetch".into()]),
            "{source}"
        );
    }
}

/// CONT-EXACT after the async adapter moves its spawn into ts.promise.start:
/// a child parks inside its source body, then finishes a nested expression.
#[test]
fn an_async_body_resumes_with_identical_values_and_charge() {
    let source = "const work = async (n: number) => n + await host.echo(n); await control.finish([await work(3), await work(4)]);";
    let plain = super::machine::plain_record_resume(source, u64::MAX, false);
    let cold = super::machine::plain_record_resume(source, 1, true);
    assert_eq!((&plain.0, plain.1), (&cold.0, cold.1));
    assert_eq!(
        plain.0,
        lash_kernel_doc::Datum::List(vec![
            lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(6.0)),
            lash_kernel_doc::Datum::Float(lash_kernel_doc::Float::new(8.0)),
        ])
    );
    assert!(
        cold.2 > 2,
        "the source body was reopened at its waits and calls"
    );
}
