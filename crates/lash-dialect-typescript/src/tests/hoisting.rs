//! `K-STMT-005`: the front end hoists in the source's evaluation order.
//!
//! `witness/hoisting/cases.tsv` holds one program per expression family.
//! Each marks its operands with calls `m.<name>(value)`, which have a side
//! effect, and `order.tsv` holds the order Node evaluated them in
//! (`record.mjs` writes it). A marked operand lowers to one statement, so
//! the statements' order in the document is the order the kernel runs them
//! in, and it must be Node's.

use std::collections::BTreeMap;

use lash_kernel_doc::{Action, Atom, Callee, Document, Literal, Node};

use super::{lower_in_session, main_text};

fn table(text: &'static str) -> BTreeMap<&'static str, &'static str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_once('\t').expect("a row is `family<TAB>value`"))
        .collect()
}

/// The names of the marker calls of a document, in statement order.
fn marked(document: &Document) -> Vec<String> {
    fn walk(node: Node<'_>, document: &Document, names: &mut Vec<String>) {
        if let Node::Action(Action::Call {
            callee: Callee::Library(function),
            args,
        }) = node
        {
            let helper = document.manifest.functions[function].as_str();
            if helper == "ts.call_member"
                && let Some(Atom::Literal(Literal::Text(name))) = args.get(1)
            {
                names.push(name.clone());
            } else if let Some(name) = helper.strip_prefix("ts.method.") {
                names.push(name.to_string());
            }
        }
        for child in node.children() {
            walk(child, document, names);
        }
    }
    let mut names = Vec::new();
    walk(Node::Block(&document.main), document, &mut names);
    names
}

#[test]
fn operands_are_hoisted_in_the_order_node_evaluates_them() {
    let cases = table(include_str!("../../witness/hoisting/cases.tsv"));
    let orders = table(include_str!("../../witness/hoisting/order.tsv"));
    assert_eq!(
        cases.keys().collect::<Vec<_>>(),
        orders.keys().collect::<Vec<_>>(),
        "run witness/hoisting/record.mjs"
    );
    for (family, source) in cases {
        let witness: Vec<&str> = orders[family].split(' ').collect();
        let lowered = match lower_in_session(source, &["m", "o"]) {
            Ok(lowered) => lowered,
            Err(diagnostic) => panic!("{family}: {diagnostic}"),
        };
        // An operand Node did not reach sits in a branch the run skips.
        let order: Vec<String> = marked(&lowered.document)
            .into_iter()
            .filter(|name| witness.contains(&name.as_str()))
            .collect();
        assert_eq!(order, witness, "{family}: {source}");
    }
}

/// A variable read before a call is copied before the call runs: the call,
/// or another task while it waits, may assign the variable.
#[test]
fn a_variable_read_before_a_call_is_held_in_a_temporary() {
    assert_eq!(
        main_text("let x = 1; function f() { x = 5; return 1; } x = x + f();"),
        "\
let x = 1.0
let f = fn(this1, args1) {
  set x = 5.0
  return 1.0
}
let t2 = x
let t3 = []
let t4 = apply f(absent, t3)
set x = invoke ts.add(t2, t4)"
    );
}

/// FIG-5779: only names that enter the session must leave built-ins intact.
#[test]
fn session_bindings_cannot_shadow_builtins() {
    for (name, source) in [
        ("URL", "const URL = 'https://example.com';"),
        ("finish", "const finish = 1;"),
        ("Math", "let Math = 1;"),
        ("Array", "var Array = 1;"),
        ("URL", "const { URL } = { URL: 1 };"),
        ("URL", "let [URL] = [1];"),
        ("URL", "function URL() { return 1; }"),
        ("URL", "enum URL { Value }"),
        ("URL", "{ var URL = 1; }"),
    ] {
        let error = super::lower(source).expect_err(source);
        assert_eq!(error.code.as_str(), "TS_SHADOWS_BUILTIN", "{error}");
        assert!(error.is_dialect_refusal());
        assert_eq!(
            error.message,
            format!("`{name}` is a built-in; a top-level binding cannot reuse its name")
        );
        assert!(
            error
                .suggestions
                .iter()
                .any(|repair| repair.contains(&format!("{name}_"))),
            "{error}"
        );
        super::lower(&source.replace(name, &format!("{name}_")))
            .expect("the renamed binding lowers");
    }
}

/// Local names never enter the session.
#[test]
fn nested_scope_builtin_shadowing_stays_local() {
    for source in [
        "function local(URL, finish) { return URL + finish; } local(1, 2);",
        "function local() { var URL = 1; return URL; } local();",
        "{ const URL = 1; const finish = 2; URL + finish; }",
        "for (let URL of [1, 2]) { URL; }",
    ] {
        super::lower(source).expect("local shadowing stays allowed");
    }
}

/// Old state, forked state and host seeds all supply the same environment.
#[test]
fn restored_session_bindings_cannot_mask_builtins() {
    for name in ["URL", "finish"] {
        let error =
            super::lower_in_session("1;", &[name]).expect_err("reject an old reserved binding");
        assert_eq!(error.code.as_str(), "TS_SHADOWS_BUILTIN");
        assert!(
            error.suggestions.iter().any(|repair| repair.contains(name)),
            "{error}"
        );
        let bindings = [lash_kernel_doc::Name::new(name)].into_iter().collect();
        let effects = std::collections::BTreeMap::new();
        let environment = lash_kernel_dialect::Environment {
            library: super::library(),
            bindings: &bindings,
            effects: &effects,
        };
        let error = crate::Parser::default()
            .lower("1;", &environment)
            .expect_err("the worker's reusable parser also checks restored names");
        assert_eq!(error.code.as_str(), "TS_SHADOWS_BUILTIN");
    }
}

/// The configured terminal name remains callable in later cells.
#[test]
fn finish_binding_cannot_shadow_the_terminal() {
    let error = super::lower("const finish = 1;").expect_err("finish is reserved");
    assert_eq!(error.code.as_str(), "TS_SHADOWS_BUILTIN");
}
