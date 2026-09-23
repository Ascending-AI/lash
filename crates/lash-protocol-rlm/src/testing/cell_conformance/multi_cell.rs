//! Axis 1: multi-cell sequences carrying real values.
//!
//! Trivial two-cell tests existed before FIG-1562 and caught nothing, because
//! the cells they ran allocated nothing worth carrying. These sequences bind
//! lists and nested records, read one cell's work from another, shadow a name,
//! grow a structure, drop a value, and put a failing cell in the middle — the
//! shapes a session actually contains.

use super::drive;
use super::harness::{HarnessMode, Session};
use super::syntax::{Cell, Literal};

#[test]
fn a_later_cell_reads_an_earlier_cells_binding() {
    let (_, model) = drive(
        HarnessMode::Resident,
        &[
            Cell::number("seed", 41.0),
            Cell::derive("answer", "seed"),
            Cell::finish("answer"),
        ],
    );
    assert_eq!(model.get("answer"), Some(&serde_json::json!(42)));
}

#[test]
fn a_binding_survives_a_long_run_of_unrelated_cells() {
    // Twelve cells of work that never mentions `kept`. Nothing in the session
    // may quietly lose it, and nothing in the session may quietly change it.
    let mut cells = vec![Cell::number("kept", 7.0)];
    for step in 0..12 {
        cells.push(Cell::bind(
            &format!("scratch{step}"),
            Literal::List(vec![step as f64, step as f64 + 1.0]),
        ));
    }
    cells.push(Cell::finish("kept"));

    let (session, _) = drive(HarnessMode::Resident, &cells);
    assert_eq!(session.globals().get("kept"), Some(&serde_json::json!(7)));
}

#[test]
fn a_later_cell_shadows_an_earlier_binding() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind("value", Literal::List(vec![1.0, 2.0])),
            // A different shape under the same name: shadowing is a rebinding,
            // not a merge, so nothing of the list may remain.
            Cell::number("value", 5.0),
        ],
    );
    assert_eq!(session.globals().get("value"), Some(&serde_json::json!(5)));
}

#[test]
fn a_later_cell_grows_a_structure_an_earlier_cell_built() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind("base", Literal::List(vec![1.0, 2.0])),
            Cell::extend("grown", "base", 3.0),
            Cell::extend("grown_again", "grown", 4.0),
        ],
    );
    let bindings = session.globals();
    assert_eq!(bindings.get("base"), Some(&serde_json::json!([1, 2])));
    assert_eq!(
        bindings.get("grown_again"),
        Some(&serde_json::json!([1, 2, 3, 4]))
    );
}

#[test]
fn a_nested_structure_crosses_the_boundary_intact() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind(
                "shape",
                Literal::Nested {
                    scalar: 3.0,
                    items: vec![4.0, 5.0, 6.0],
                },
            ),
            Cell::closure_garbage("noise"),
            Cell::number("tail", 1.0),
        ],
    );
    assert_eq!(
        session.globals().get("shape"),
        Some(&serde_json::json!({ "scalar": 3, "items": [4, 5, 6] }))
    );
}

#[test]
fn a_dropped_binding_keeps_its_name_and_loses_its_value() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind("payload", Literal::List(vec![1.0, 2.0, 3.0])),
            Cell::drop_value("payload"),
            Cell::number("after", 1.0),
        ],
    );
    assert_eq!(
        session.globals().get("payload"),
        Some(&serde_json::Value::Null),
        "dropping a value must not delete the name a later cell may still read"
    );
}

#[test]
fn a_failing_cell_between_two_working_cells_changes_nothing() {
    // `drive` already checks the model after the failing cell; this scenario
    // exists to pin the sequence itself as a named case, because it is the one
    // a host reported.
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind("before", Literal::List(vec![1.0, 2.0])),
            Cell::CompileError,
            Cell::RuntimeError,
            Cell::extend("after", "before", 3.0),
        ],
    );
    assert_eq!(
        session.globals().get("after"),
        Some(&serde_json::json!([1, 2, 3]))
    );
}

#[test]
fn the_session_finishes_with_a_value_an_earlier_cell_bound() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok(&Cell::number("answer", 42.0).render());
    session.run_ok(&Cell::closure_garbage("noise").render());
    let outcome = session.run_ok(&Cell::finish("answer").render());
    assert_eq!(outcome.finish, Some(serde_json::json!(42)));
}

/// A closure bound to a session global does not cross the cell boundary.
///
/// This is the ruled contract, not an accident: a closure's function index only
/// means something inside the program that compiled it, and the exported view
/// of the globals already dropped any binding that reached a function value, so
/// the runtime roots now match it. See
/// `docs/adr/0076-lashlang-durable-stores-hold-exclusively-owned-copies.md`.
#[test]
fn a_closure_valued_binding_does_not_survive_the_cell_boundary() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::closure_binding("callback"),
            Cell::number("sibling", 3.0),
        ],
    );
    let bindings = session.globals();
    assert!(
        !bindings.contains_key("callback"),
        "a closure-valued binding reached the next cell: {bindings:?}"
    );
    assert_eq!(bindings.get("sibling"), Some(&serde_json::json!(3)));
}

/// The boundary asks a reachability question, not a shallow type question: a
/// closure inside a container takes the container with it.
#[test]
fn a_container_reaching_a_closure_does_not_survive_either() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok("const handlers = { onDone: (value: number) => value + 1 };\nconst tag = 5;");
    let bindings = session.globals();
    assert!(
        !bindings.contains_key("handlers"),
        "a container reaching a closure survived the boundary: {bindings:?}"
    );
    assert_eq!(
        bindings.get("tag"),
        Some(&serde_json::json!(5)),
        "the rest of the cell's bindings are untouched"
    );
    assert_eq!(
        session.run_ok("finish(tag);").finish,
        Some(serde_json::json!(5))
    );
}

/// A closure-valued binding is shadowed by an ordinary value in a later cell.
///
/// The interesting part is the order: the closure cell runs first, so if its
/// binding had survived, the shadowing cell would be rebinding a name the
/// runtime roots still reach a closure through.
#[test]
fn an_ordinary_value_shadows_a_closure_valued_binding() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::closure_binding("slot"),
            Cell::number("slot", 11.0),
            Cell::derive("read", "slot"),
        ],
    );
    assert_eq!(session.globals().get("read"), Some(&serde_json::json!(12)));
}
