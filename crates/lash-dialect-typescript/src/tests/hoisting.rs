//! `K-EVAL-001`: the front end hoists in the source's evaluation order.
//!
//! The Node witness records one side-effect order per expression family.
//! The current execution law observes those effects in guest code rather
//! than inspecting a particular lowering helper.

use std::collections::BTreeMap;

use lash_kernel_doc::Datum;

fn table(text: &'static str) -> BTreeMap<&'static str, &'static str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_once('\t').expect("a row is `family<TAB>value`"))
        .collect()
}

/// K-EVAL-001: each expression runs its reached operands in Node's order.
#[test]
fn expression_side_effects_follow_node_order() {
    use super::machine::{Ended, end};

    let cases = table(include_str!("../../witness/hoisting/cases.tsv"));
    let orders = table(include_str!("../../witness/hoisting/order.tsv"));
    assert_eq!(
        cases.keys().collect::<Vec<_>>(),
        orders.keys().collect::<Vec<_>>()
    );
    for (family, source) in cases {
        let names: std::collections::BTreeSet<_> = source
            .split("m.")
            .skip(1)
            .map(|suffix| suffix.split('(').next().expect("a marker call"))
            .collect();
        let methods: Vec<_> = names
            .into_iter()
            .map(|name| {
                format!("{name}(value) {{ order += (order ? ' ' : '') + '{name}'; return value; }}")
            })
            .collect();
        let program = format!(
            "let order = ''; const m = {{{}}}; const o = {{id(value) {{ return value; }}, n: 0}}; {source} await control.finish(order);",
            methods.join(",")
        );
        assert_eq!(
            end(&program),
            Ended::Finished(Datum::Text(orders[family].into())),
            "{family}: {source}"
        );
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
