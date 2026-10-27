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

/// FIG-7323: ordinary arguments carry no builtin dispatch graph, including
/// through a packed invocation or an unawaited tool's child task.
#[test]
fn only_arguments_that_can_contain_builtins_have_a_forwarder() {
    use lash_kernel_dialect::Library;

    let forward = super::library().resolve("ts.builtin_call").unwrap();
    for source in [
        "await host.echo(1);",
        "const x: number = 1; await host.echo(x);",
        "const x = 1; await host.echo({x, nested: [null, 'text', x + 2]});",
        "await host.echo(...[1]);",
        "const pending = host.echo({x: 1}); await pending;",
        "function f(x: number) { return host.echo(x); } await f(1);",
        "const f = async (x: number) => x; const alias = f; await host.echo({list: [1, alias]});",
        "const n = num.checked_float(1); await host.echo({n, sum: num.add(n, 2)});",
    ] {
        let lowered = lower_with_effects(source).unwrap();
        assert!(
            !lowered.document.library.contains(&forward),
            "no builtin forwarding work: {source}"
        );
    }
    for source in [
        "await host.echo(Math.abs);",
        "await host.echo({nested: [Math.abs]});",
        "const xs = [Math.abs]; await host.echo(...xs);",
        "function f(x: any) { return host.echo(x); } await f(Math.abs);",
        "const raw = {n: 1, f: Math.abs}; const value: {n: number} = raw; await host.echo(value);",
        "const num = {checked_float: () => Math.abs}; await host.echo(num.checked_float());",
    ] {
        let lowered = lower_with_effects(source).unwrap();
        assert!(
            lowered.document.library.contains(&forward),
            "a possible builtin keeps its forwarding factory: {source}"
        );
    }
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

/// A tool is called with the arguments its signature takes.
#[test]
fn a_tool_call_with_the_wrong_number_of_arguments_is_refused() {
    let error = lower_with_effects("await host.echo();").expect_err("too few");
    assert_eq!(error.code(), DiagnosticCode::UnsupportedExpression);
    assert!(lower_with_effects("await host.echo(1, 2);").is_err());
}

/// FN-CROSS: nested guest functions freeze, and builtin callables cross as
/// finite source-owned wrappers with their declared arity. Namespaces refuse.
#[test]
fn tool_arguments_freeze_nested_functions_and_forward_builtin_callables() {
    use lash_kernel_doc::{Datum, Float};

    use super::machine::{Ended, end};

    let direct = end("const value = () => 1; await control.finish(value);");
    let Ended::Finished(Datum::FunctionValue(function)) = direct else {
        panic!("a guest function crosses as a function value: {direct:?}");
    };
    let nested = end("const value = () => 1; await control.finish([1, [2, value], value]);");
    let Ended::Finished(Datum::List(mut values)) = nested else {
        panic!("nested guest functions cross as frozen values: {nested:?}");
    };
    let Datum::FunctionValue(nested_function) = values.pop().expect("the direct function") else {
        panic!("the last value is the same function passed inside the array");
    };
    assert_eq!(nested_function.signature, function.signature);
    assert_eq!(nested_function.captures, function.captures);
    assert_eq!(
        values,
        vec![
            Datum::Float(Float::new(1.0)),
            Datum::List(vec![
                Datum::Float(Float::new(2.0)),
                Datum::FunctionValue(nested_function)
            ]),
        ]
    );
    assert_eq!(
        end("await control.finish(Math);"),
        Ended::Raised("not_data".to_string())
    );
    for (argument, arity) in [("Math.abs", 1), ("Math.max", 2)] {
        let source = format!("await control.finish([{argument}, {{f: {argument}}}]);");
        let Ended::Finished(Datum::List(values)) = end(&source) else {
            panic!("a builtin callable crosses inside data: {argument}");
        };
        let Datum::FunctionValue(function) = &values[0] else {
            panic!("a builtin crosses as a frozen wrapper");
        };
        let lowered = lower_with_effects(&source).unwrap();
        assert!(
            lowered
                .document
                .functions
                .contains_key(&function.callable.function)
        );
        assert_eq!(function.signature.params.len(), arity);
        assert!(matches!(
            function.convention,
            lash_kernel_doc::StartConvention::TsArgsList { .. }
        ));
        assert_eq!(
            values[1],
            Datum::Record(vec![("f".into(), Datum::FunctionValue(function.clone()))])
        );
    }
}

/// `new Promise(executor)` stays refused: a promise is settled by its
/// task's end, and nothing else can end a task.
#[test]
fn the_promise_constructor_is_refused() {
    let error = lower("new Promise((resolve) => resolve(1));").expect_err("no constructor");
    assert_eq!(error.code(), DiagnosticCode::NewUnsupported);
}

/// FIG-6524: only free callees are tool candidates, at their lexical scope.
#[test]
fn tool_discovery_resolves_lexical_values_before_namespaces() {
    let bindings =
        std::collections::BTreeSet::from([lash_kernel_value::names::Name::new("previous")]);
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
        calls.calls,
        std::collections::BTreeSet::from(["items.sort".into(), "other.fetch".into()])
    );
}

/// FIG-6556: discovery is lexical even when lowering refuses earlier code;
/// bare unknown names never become deferred tool authority.
#[test]
fn tool_discovery_survives_refusals_and_ignores_bare_calls() {
    for source in [
        "Math.random(); workflows_create(); await tools.lookup({ nested: other.fetch({}) });",
        "(1).toLocaleString(\"de-DE\"); workflows_create(); await tools.lookup({ nested: other.fetch({}) });",
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
            calls.calls,
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

/// FIG-7142: a discovered namespace cannot reinterpret a persistent local.
#[test]
fn a_cell_local_shadowing_a_discovered_namespace_is_diagnosed() {
    use lash_kernel_dialect::{CellEnvironment, Environment, Mode, Namespaces};
    use std::collections::{BTreeMap, BTreeSet};
    use {lash_kernel_value::Name, lash_kernel_value::PackagePath};

    let deferred = Namespaces::new(
        BTreeMap::new(),
        BTreeSet::from([PackagePath::parse("jev").unwrap()]),
        &|_| false,
    )
    .unwrap();
    let offered = super::linked::namespaces(
        &BTreeMap::from([(
            lash_kernel_value::EffectName::new("jev.decide").unwrap(),
            lash_kernel_value::Signature {
                params: vec![lash_kernel_value::Param {
                    name: Name::new("input"),
                    ty: lash_kernel_value::Type::Any,
                    optional: false,
                }],
                result: lash_kernel_value::Type::Any,
            },
        )]),
        &BTreeMap::new(),
    )
    .unwrap();
    for namespaces in [deferred, offered] {
        let bindings = BTreeSet::from([Name::new("jev")]);
        let functions = BTreeMap::new();
        let environment = Environment::new(super::library(), &namespaces);
        let source = "await jev.decide({});";
        let lower = |source, cell| {
            crate::Parser::default()
                .lower(source, &environment, Mode::Cell(cell))
                .map_err(lash_kernel_dialect::Diagnostic::from)
        };
        let error = lower(
            source,
            CellEnvironment {
                bindings: &bindings,
                functions: &functions,
            },
        )
        .expect_err("shadowing is diagnosed before execution");
        assert_eq!(error.code(), "NAMESPACE_SHADOWED_BY_LOCAL");
        assert!(error.repairs.is_empty());
        let lash_kernel_dialect::DiagnosticReason::NamespaceShadowing { shadowing, .. } =
            &error.reason
        else {
            panic!("typed namespace facts");
        };
        assert_eq!(shadowing.binding_name, "jev");
        assert_eq!(shadowing.binding_origin, None); // CellEnvironment has names, not cell provenance.
        assert_eq!(shadowing.namespace, "jev");
        assert_eq!(shadowing.call_path, "jev.decide");
        assert_eq!(error.span().unwrap().start, 6);
        let tool =
            lower(source, CellEnvironment::first()).expect("without the local the tool resolves");
        assert!(
            tool.document
                .imports
                .values()
                .any(|import| import.name.to_string() == "jev.decide")
        );
        for source in [
            "const jev = {}; await jev.decide({});",
            "const jev: any = {}; await jev.decide({});",
        ] {
            let error = lower(source, CellEnvironment::first())
                .expect_err("types do not change lexical resolution");
            assert_eq!(error.code(), "NAMESPACE_SHADOWED_BY_LOCAL");
            let lash_kernel_dialect::DiagnosticReason::NamespaceShadowing { shadowing, .. } =
                &error.reason
            else {
                panic!("typed namespace facts");
            };
            assert_eq!(shadowing.binding_name, "jev");
        }
        lower(
            "const jev = 41; console.log(jev + 1);",
            CellEnvironment::first(),
        )
        .expect("a namespace root can be used as an ordinary local");
        lower(
            "console.log(jev + 1);",
            CellEnvironment {
                bindings: &bindings,
                functions: &functions,
            },
        )
        .expect("a carried local keeps its ordinary value meaning");
        let empty = Namespaces::default();
        crate::Parser::default()
            .lower(
                "const jev = { decide: () => 1 }; jev.decide();",
                &Environment::new(super::library(), &empty),
                Mode::Cell(CellEnvironment::first()),
            )
            .expect("a local method with no namespace stays local");
    }
}

/// FIG-7284 / K-EVAL-004: expand operands once and validate before requesting an effect.
#[test]
fn spread_effect_arguments_are_evaluated_once() {
    let recorded = run(
        r#"
        let evaluations = 0;
        function once(value) { evaluations++; return value; }
        const value = await host.echo(...once(["spread"]));
        const promise = host.echo(...once(["promise"]));
        const other = await promise;
        await sleep(...once([0]));
        let failures = 0;
        try { await host.echo(...once([])); } catch (e) { failures++; }
        try { await host.echo(...once([1, 2])); } catch (e) { failures++; }
        try { await sleep(...once([])); } catch (e) { failures++; }
        console.log(value + ":" + other + ":" + evaluations + ":" + failures);
    "#,
        &[
            vec!["host.echo:spread".into()],
            vec!["host.echo:promise".into()],
            vec!["sleep:0".into()],
        ],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), vec!["spread:promise:6:3"]);
    let asked: Vec<_> = recorded
        .epochs
        .iter()
        .flat_map(|epoch| epoch.asked.iter().map(String::as_str))
        .collect();
    assert_eq!(
        asked,
        vec!["host.echo:spread", "host.echo:promise", "sleep:0"]
    );
}

/// FIG-7284 / K-TASK-018: completed discarded work is legal; unfinished work still fails.
#[test]
fn discarded_tool_promises_are_judged_at_root_cleanup() {
    let recorded = run(
        "host.echo('done'); await sleep(0);",
        &[vec!["host.echo:done".into(), "sleep:0".into()]],
    );
    assert_eq!(recorded.end, "ok");
    assert!(lower_with_effects("void host.echo(1);").is_ok());
    let outstanding = run("host.echo('pending');", &[]);
    assert!(
        outstanding.end.contains("outstanding"),
        "{}",
        outstanding.end
    );
}
