//! The workflow-graph lens's print → reparse → admit round trip over the
//! shapes FIG-3635 and FIG-3663 found open: hoisted `var` declarations
//! beside function declarations, and opaque nodes that read the session's
//! globals.

use std::collections::BTreeSet;

use lash_typescript::parse;
use lash_typescript::workflow_graph::{
    typescript_program_source, workflow_graph_from_source, workflow_graph_to_source,
    workflow_graph_to_source_in_session,
};
use lashlang::WorkflowNodeKind;

fn canonical(source: &str) -> String {
    typescript_program_source(&parse(source).expect("fixture parses"))
        .expect("a parsed fixture prints back as TypeScript")
}

/// The language-neutral IR projection, with TypeScript opaque-statement text.
fn workflow_graph_from_program(program: &lashlang::Program) -> lashlang::WorkflowGraph {
    lashlang::workflow_graph_from_program(
        program,
        &lash_typescript::workflow_graph::TypeScriptStatementText,
    )
}

/// Every lens law over one fixture.
fn assert_lens_laws(source: &str) {
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");
    let rendered = workflow_graph_to_source(&graph).expect("graph renders");
    assert_eq!(rendered, canonical, "GetPut");
    assert_eq!(
        parse(&rendered).expect("rendered source parses"),
        parse(&canonical).expect("canonical source parses"),
    );
    assert_eq!(
        workflow_graph_from_source(&rendered).expect("rendered source reprojects"),
        graph,
        "PutGet",
    );
}

#[test]
fn hoisted_vars_and_function_declarations_round_trip() {
    // FIG-3635: the lowerer hoists each top-level `var` to a `name =
    // undefined` assignment ahead of the statements hoisted function
    // declarations flush to. Only `var` spells that order back — `let x =
    // undefined` stays where it is printed, behind the declaration.
    assert_lens_laws(
        "var x = function () {\n  return 1;\n};\nvar y = function () {\n  return 2;\n};\nfunction f_arg() {}\nf_arg();\nfinish(x);\n",
    );
}

#[test]
fn hoisted_vars_round_trip_through_global_this() {
    // The same hoisting, with the function reading the `var` through the
    // session global `globalThis` addresses it as.
    assert_lens_laws("var x = 1;\nfunction f() {\n  return globalThis.x;\n}\nfinish(f());\n");
}

#[test]
fn var_initialized_to_a_function_round_trips() {
    // `var x = function () {}` is a hoist plus an initializer, which the
    // `var` declaration carries back in one statement.
    assert_lens_laws("var x = function () {\n  return 1;\n};\nfinish(x());\n");
}

#[test]
fn opaque_statements_read_session_globals() {
    // FIG-3663: an opaque statement may read a session global the program
    // itself linked against — the corpus's Test262 cells throw
    // `Test262Error`, which the session bound before the cell ran.
    let globals: BTreeSet<String> = ["Test262Error".to_string()].into_iter().collect();
    let program = lash_typescript::parse_with_globals("throw Test262Error(\"no\");\n", &globals)
        .expect("source parses against the session");
    let graph = workflow_graph_from_program(&program);
    let rendered = workflow_graph_to_source_in_session(&graph, &globals)
        .expect("the graph renders against the session's globals");
    assert_eq!(rendered, "throw Test262Error(\"no\");\n");
    assert!(
        workflow_graph_to_source(&graph).is_err(),
        "without the session's globals the opaque source still refuses"
    );
}

#[test]
fn opaque_statements_reject_globals_the_program_cannot_see() {
    // The session's globals add the names the cell linked against, no more:
    // an opaque statement that reads anything else still fails its reparse.
    let globals: BTreeSet<String> = ["Test262Error".to_string()].into_iter().collect();
    let program = lash_typescript::parse_with_globals("throw Test262Error(\"no\");\n", &globals)
        .expect("source parses against the session");
    let mut graph = workflow_graph_from_program(&program);
    let source = graph
        .main
        .nodes
        .iter_mut()
        .find_map(|node| match &mut node.kind {
            WorkflowNodeKind::Opaque { source } => Some(source),
            _ => None,
        })
        .expect("the program projects an opaque node");
    *source = "throw NotBoundHere(\"no\");".to_string();
    let error = workflow_graph_to_source_in_session(&graph, &globals)
        .expect_err("a name in neither the node nor the session refuses");
    assert_eq!(error.code(), "invalid_opaque_source");
}

#[test]
fn classic_for_prints_back_in_every_head_condition_and_update_form() {
    // FIG-3706: a classic `for` lowers to its head's statements and a
    // `while` whose body ends with the update; the lens prints that shape
    // back as the loop, and a `continue` (the update, then the jump) as the
    // bare `continue`.
    let cases = [
        "for (let i = 0, j = 10; (i < j); i++) {\n  j = (j - 1);\n  if ((i === 2)) {\n    continue;\n  }\n}\nfinish(1);\n",
        "let k = 0;\nfor (k = 3; (k > 0); k--) {}\nfinish(k);\n",
        "var v;\nfor (var v = 7;;) {\n  break;\n}\nfinish(v);\n",
        "for (;;) {\n  break;\n}\nfinish(1);\n",
        "for (let i = 0; (i < 9); i = (i * 2)) {}\nfinish(1);\n",
        "for (let i = 0, f = () => (i); (i < 3); i++) {\n  for (let n = 0;; n++) {\n    if ((n > i)) {\n      break;\n    }\n    continue;\n  }\n}\nfinish(1);\n",
    ];
    for source in cases {
        assert_eq!(canonical(source), source, "the loop prints back as itself");
        assert_lens_laws(source);
    }
}
