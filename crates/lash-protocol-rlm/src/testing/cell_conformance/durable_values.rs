//! Axis 5: every value a session can hold survives the cell boundary and a
//! durable reload exactly as Node keeps it (FIG-3605, FIG-3606).
//!
//! Each scenario binds a value in one cell, changes it in a later one, and
//! reads it back in a third. It runs twice — resident, and restarting through
//! the production capture between every pair of cells — and both runs must
//! finish with the value Node v25.2.1 finishes with when the same cells run as
//! successive classic Scripts in one realm. The Node answers are recorded
//! below; each was produced that way.
//!
//! The values are the ones the host view cannot carry: the TypeScript exotics
//! (`Map`, `Set`, `Date`, `RegExp` and its match, `URL`, `URLSearchParams`),
//! one object named by two bindings, and an object's property order.

use serde_json::json;

use super::harness::{HarnessMode, Session};

/// Runs `cells` in `mode` and returns what the last one finished with.
fn finished_with(mode: HarnessMode, cells: &[&str]) -> serde_json::Value {
    let (last, earlier) = cells.split_last().expect("a scenario has cells");
    let mut session = Session::open(mode);
    for cell in earlier {
        session.run_ok(cell);
    }
    session
        .run_ok(last)
        .finish
        .unwrap_or_else(|| panic!("the last cell of {cells:?} must finish"))
}

/// Live, reloaded, and Node agree.
fn assert_matches_node(cells: &[&str], node: serde_json::Value) {
    for mode in HarnessMode::ALL {
        assert_eq!(
            finished_with(*mode, cells),
            node,
            "{mode:?} diverged from Node for {cells:?}"
        );
    }
}

#[test]
fn a_map_is_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            r#"const seen = new Map([["a", 1]]);"#,
            r#"seen.set("b", 2);"#,
            r#"finish([Array.from(seen), seen.size, seen.get("a")]);"#,
        ],
        json!([[["a", 1], ["b", 2]], 2, 1]),
    );
}

#[test]
fn a_set_is_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            r#"const tags = new Set(["x"]);"#,
            "tags.add(\"y\");\ntags.add(\"x\");",
            r#"finish([Array.from(tags), tags.size, tags.has("y")]);"#,
        ],
        json!([["x", "y"], 2, true]),
    );
}

#[test]
fn a_date_is_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            "const when = new Date(Date.UTC(2020, 0, 2, 3, 4, 5));",
            "finish([when.toISOString(), when.getTime(), when.getUTCDate()]);",
        ],
        json!(["2020-01-02T03:04:05.000Z", 1_577_934_245_000_u64, 2]),
    );
}

/// `lastIndex` is state a global RegExp carries between calls, and the match
/// an earlier cell bound is an exotic of its own.
#[test]
fn a_regexp_and_its_match_are_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            "const pattern = /a(b+)/g;",
            r#"const first = pattern.exec("abbxab");"#,
            r#"finish([pattern.lastIndex, first[0], first[1], first.index, pattern.exec("abbxab")[0], pattern.source, pattern.flags]);"#,
        ],
        json!([3, "abb", "bb", 0, "ab", "a(b+)", "g"]),
    );
}

#[test]
fn a_url_is_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            r#"const site = new URL("https://example.com/a?x=1");"#,
            r#"site.searchParams.append("y", "2");"#,
            r#"finish([site.href, site.search, site.searchParams.get("y")]);"#,
        ],
        json!(["https://example.com/a?x=1&y=2", "?x=1&y=2", "2"]),
    );
}

#[test]
fn url_search_params_are_live_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            r#"const query = new URLSearchParams("a=1&b=2");"#,
            "query.set(\"a\", \"3\");\nquery.append(\"c\", \"4\");",
            r#"finish([query.toString(), query.get("a")]);"#,
        ],
        json!(["a=3&b=2&c=4", "3"]),
    );
}

/// `Object.keys`, `JSON.stringify` and `for...in` read property order, so a
/// reload that sorted the keys changed what the program computes.
#[test]
fn property_order_is_insertion_order_in_later_cells_and_across_reloads() {
    assert_matches_node(
        &[
            "const ordered = { zeta: 1, alpha: 2, mid: 3 };\nconst nested = { b: { y: 1, x: 2 }, a: [{ q: 1, p: 2 }] };",
            "ordered.beta = 4;",
            "finish([Object.keys(ordered), JSON.stringify(ordered), JSON.stringify(nested)]);",
        ],
        json!([
            ["zeta", "alpha", "mid", "beta"],
            r#"{"zeta":1,"alpha":2,"mid":3,"beta":4}"#,
            r#"{"b":{"y":1,"x":2},"a":[{"q":1,"p":2}]}"#
        ]),
    );
}

#[test]
fn two_bindings_to_one_object_stay_one_object() {
    assert_matches_node(
        &[
            "const a = { x: 1 };\nconst b = a;",
            "b.x = 2;",
            "finish([a.x, b.x, a === b]);",
        ],
        json!([2, 2, true]),
    );
}

/// A write through a binding a later cell made is a write to the earlier
/// cell's object, which no assignment to the earlier binding records.
#[test]
fn a_write_through_a_later_alias_reaches_the_earlier_binding() {
    assert_matches_node(
        &[
            "const holder = { list: [1] };",
            "const list = holder.list;\nlist.push(9);",
            "finish([holder.list, list === holder.list]);",
        ],
        json!([[1, 9], true]),
    );
}

#[test]
fn an_object_shared_into_a_map_stays_shared() {
    assert_matches_node(
        &[
            "const cfg = { n: 1 };\nconst registry = new Map([[\"cfg\", cfg]]);",
            "cfg.n = 5;",
            r#"finish([registry.get("cfg").n, registry.get("cfg") === cfg]);"#,
        ],
        json!([5, true]),
    );
}
