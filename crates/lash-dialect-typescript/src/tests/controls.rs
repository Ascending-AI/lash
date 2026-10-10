//! Control calls: a call of a tool that ends the turn (FIG-5781).
//!
//! A cell ends its turn only by awaiting a control call where it stands, at
//! the top level of the cell: `main` ends as soon as that call settles, so
//! nothing after it runs. A control call anywhere else could settle beside
//! work still running, and is refused before anything runs.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{EffectControl, Environment, Lowered};
use lash_kernel_doc::{EffectName, Name, Param, Signature, Stmt, Type};

use super::machine;
use crate::Diagnostic;

/// Lowers `source` against `control.finish`, a tool that ends the turn,
/// and `echo`, one that does not.
fn lower_against_control(source: &str) -> Result<Lowered, Diagnostic> {
    let signature = Signature {
        params: vec![Param {
            name: Name::new("x"),
            ty: Type::Any,
            optional: false,
        }],
        result: Type::Any,
    };
    let finish = EffectName::new("control.finish").expect("a tool's name");
    let effects = BTreeMap::from([
        (finish.clone(), signature.clone()),
        (EffectName::new("echo").expect("a tool's name"), signature),
    ]);
    let controls = BTreeMap::from([(finish, BTreeSet::from([EffectControl::Finish]))]);
    crate::lower(
        source,
        &Environment {
            library: super::library(),
            effects: &effects,
            controls: &controls,
            tool_roots: &std::collections::BTreeSet::new(),
            bindings: &BTreeSet::new(),
            functions: &BTreeMap::new(),
        },
    )
}

fn ends_main(lowered: &Lowered) -> bool {
    lowered
        .document
        .main
        .iter()
        .any(|stmt| matches!(stmt, Stmt::Finish { .. }))
}

/// Law 1: the cell ends where its control call settles. The print after it
/// never runs, and the call is the last thing the cell asked of its host.
#[test]
fn a_control_call_ends_the_cell_and_nothing_after_it_runs() {
    let recorded = machine::run(
        "console.log(\"before\");\nawait finish(1);\nconsole.log(\"after\");",
        &[],
    );
    assert_eq!(recorded.lines(), ["before"]);
    assert_eq!(recorded.end, "ok");
    let asked: Vec<&str> = recorded
        .epochs
        .iter()
        .flat_map(|epoch| epoch.asked.iter().map(String::as_str))
        .collect();
    assert_eq!(asked, ["finish:1"]);
}

/// Law 2: a control call that is not awaited where it stands at the top of
/// the cell (in `Promise.all`, kept as a promise, or inside a function) is
/// refused at lowering, so no tool call of the cell runs.
#[test]
fn a_control_call_not_awaited_at_the_top_level_is_refused_before_anything_runs() {
    for source in [
        "await Promise.all([control.finish(1), echo(2)]);",
        "const pending = control.finish(1);\nawait echo(2);\nawait pending;",
        "async function end() { await control.finish(1); }\nawait end();",
        "await Promise.all([1, 2].map(async (x) => await control.finish(x)));",
    ] {
        let error = lower_against_control(source).expect_err(source);
        assert_eq!(error.code.as_str(), "TS_CONTROL_CALL_PLACEMENT", "{source}");
        assert!(!error.is_dialect_refusal(), "a program defect: {source}");
    }
    let lowered = lower_against_control("await echo(1);\nawait control.finish(2);")
        .expect("a control call awaited at the top level lowers");
    assert!(ends_main(&lowered));
}

/// FIG-5781: a tool namespace root is reserved. A cell that binds
/// `control` would hide `control.finish` from itself and every later cell,
/// so the binding is refused with a repair, and nothing is performed; a
/// session that holds such a binding from before is refused the same way.
#[test]
fn a_cell_cannot_bind_a_tool_namespace_root() {
    for source in [
        "const control = { finish: (x) => x * 2 };\nconsole.log(control.finish(21));",
        "function control() { return 1; }\nconsole.log(control());",
    ] {
        let error = lower_against_control(source).expect_err(source);
        assert_eq!(error.code.as_str(), "TS_SHADOWS_BUILTIN", "{source}");
        assert!(
            error
                .suggestions
                .iter()
                .any(|repair| repair.contains("control_")),
            "{error}"
        );
    }
    // A name inside a function is its own, and another root is free.
    lower_against_control("function f() { const control = 1; return control; }\nconsole.log(f());")
        .expect("a function's own binding lowers");
    let tool = lower_against_control("await control.finish(42);").expect("the tool call lowers");
    assert!(ends_main(&tool));
}

/// Law 14: only the trusted kernel-text entry reads kernel text. A model's
/// cell written as kernel text is ordinary TypeScript, in which `k` is the
/// reserved kernel namespace, so it cannot reach the kernel's `finish`.
#[test]
fn model_cells_cannot_reach_the_kernel_finish() {
    let source = "let x = k.add(k.int(\"1\"), k.float(\"2.0\")); k.finish(k.tuple(x, k.absent));";
    let error = super::lower(source).expect_err("a model cell does not read kernel text");
    assert_eq!(error.code.as_str(), "TS_RESERVED_IDENTIFIER", "{error}");
    let environment = Environment {
        library: super::library(),
        effects: &BTreeMap::new(),
        controls: &BTreeMap::new(),
        tool_roots: &std::collections::BTreeSet::new(),
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    let lowered = crate::lower_kernel_text(source, &environment).expect("the host entry reads it");
    assert!(ends_main(&lowered));
}
