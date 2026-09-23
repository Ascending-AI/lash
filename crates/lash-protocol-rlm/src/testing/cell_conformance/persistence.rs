//! Axis 4: the cross-cell laws composed with snapshot and restore.
//!
//! A law that only holds while the process stays up is not a law of the
//! session. Everything the other axes assert is asserted again here across the
//! durability boundary, in two shapes: an explicit mid-session snapshot after
//! closure-bearing cells followed by *different* cells, and a harness mode that
//! restarts between every pair of cells.
//!
//! The restart is the production path — the same capture the runtime persists,
//! hydrated back through the same restore a rehydrating worker uses — so a
//! divergence between the two modes is a real divergence between a session that
//! stayed on one worker and one that did not.

use super::drive;
use super::harness::{HarnessMode, Session};
use super::syntax::{Cell, Literal};

/// A representative session: real values, a closure-bearing cell, a failing
/// cell, and reads of earlier work. Used wherever a scenario needs "a session",
/// so the two modes are compared over the same history.
fn representative_session() -> Vec<Cell> {
    let mut cells = vec![
        Cell::bind("base", Literal::List(vec![1.0, 2.0, 3.0])),
        Cell::bind(
            "shape",
            Literal::Nested {
                scalar: 3.0,
                items: vec![4.0, 5.0],
            },
        ),
        Cell::closure_garbage("scaled"),
        Cell::CompileError,
        Cell::extend("grown", "base", 4.0),
        Cell::number("counter", 1.0),
        Cell::derive("counter_next", "counter"),
        Cell::RuntimeError,
        Cell::drop_value("shape"),
    ];
    cells.insert(3, Cell::closure_binding("callback"));
    cells
}

/// Snapshot after closure-bearing cells, restore, then run different cells.
///
/// The cells after the restore are deliberately not the cells before it: the
/// defect this composes against was a closure being validated against a program
/// that never compiled it, so replaying the same program would not touch it.
#[test]
fn a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok(&Cell::bind("base", Literal::List(vec![1.0, 2.0])).render());
    session.run_ok(&Cell::closure_garbage("scaled").render());
    session.run_ok(&Cell::closure_binding("callback").render());
    let before = session.globals();

    session.restart();
    assert_eq!(
        session.globals(),
        before,
        "restoring must not change what the session holds"
    );

    // Different cells, none of which the pre-snapshot programs contained.
    session.run_ok(&Cell::extend("grown", "base", 3.0).render());
    let outcome = session.run_ok(&Cell::finish("grown").render());
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2, 3])));
}

/// Restarting between every pair of cells changes nothing a cell can see.
#[test]
fn restarting_between_every_pair_of_cells_preserves_the_session() {
    let cells = representative_session();
    let (resident, _) = drive(HarnessMode::Resident, &cells);
    let (restarting, _) = drive(HarnessMode::RestartBetweenCells, &cells);
    assert_eq!(
        resident.globals(),
        restarting.globals(),
        "a session that survived a restart between every cell must hold what a resident one holds"
    );
}

/// The persisted state is byte-for-byte the same either way.
///
/// Equal *sizes* would be the weak version of this, and it is not the claim
/// worth having: if a restart changed the encoding without changing its length
/// — a reordered map, a re-keyed leaf, a differently split component — two
/// workers would write different snapshots for the same session and every later
/// comparison of them would be noise. So the root record and every leaf body are
/// compared directly.
#[test]
fn a_restarted_session_persists_the_same_bytes() {
    let cells = representative_session();
    let (resident, _) = drive(HarnessMode::Resident, &cells);
    let (restarting, _) = drive(HarnessMode::RestartBetweenCells, &cells);

    let resident = resident.persisted_state();
    let restarting = restarting.persisted_state();
    assert_eq!(
        resident.root, restarting.root,
        "a restart must not change the persisted root record"
    );
    assert_eq!(
        resident.components.keys().collect::<Vec<_>>(),
        restarting.components.keys().collect::<Vec<_>>(),
        "a restart must not change which leaves the session persists"
    );
    for (key, body) in &resident.components {
        assert_eq!(
            Some(body),
            restarting.components.get(key),
            "a restart changed the persisted body of leaf `{key}`"
        );
    }
}

/// A snapshot taken right after a cell failed restores into a usable session.
#[test]
fn a_snapshot_after_a_failing_cell_restores_cleanly() {
    let (mut session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::bind("kept", Literal::List(vec![1.0, 2.0])),
            Cell::closure_garbage("scaled"),
            Cell::RuntimeError,
        ],
    );
    let before = session.globals();
    session.restart();
    assert_eq!(session.globals(), before);
    let outcome = session.run_ok(&Cell::finish("kept").render());
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2])));
}

/// Restoring twice in a row is the same as restoring once.
///
/// A rehydrating worker can lose its lease and hand the session on again before
/// running anything, so a restore has to be a fixed point.
#[test]
fn restoring_twice_without_running_a_cell_is_a_fixed_point() {
    let (mut session, _) = drive(HarnessMode::Resident, &representative_session());
    let bindings = session.globals();
    let persisted = session.persisted_state();
    session.restart();
    session.restart();
    session.restart();
    assert_eq!(session.globals(), bindings);
    let after = session.persisted_state();
    assert_eq!(after.root, persisted.root);
    assert_eq!(after.components, persisted.components);
    let outcome = session.run_ok(&Cell::finish("grown").render());
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2, 3, 4])));
}
