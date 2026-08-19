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
use super::harness::{Dialect, HarnessMode, Session};
use super::syntax::{Cell, Literal};

/// A representative session: real values, a closure-bearing cell, a failing
/// cell, and reads of earlier work. Used wherever a scenario needs "a session",
/// so the two modes are compared over the same history.
fn representative_session(dialect: Dialect) -> Vec<Cell> {
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
    if Cell::closure_binding("callback").expressed_by(dialect) {
        cells.insert(3, Cell::closure_binding("callback"));
    }
    cells
}

/// Snapshot after closure-bearing cells, restore, then run different cells.
///
/// The cells after the restore are deliberately not the cells before it: the
/// defect this composes against was a closure being validated against a program
/// that never compiled it, so replaying the same program would not touch it.
fn a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells(dialect: Dialect) {
    let mut session = Session::open(dialect, HarnessMode::Resident);
    session.run_ok(&Cell::bind("base", Literal::List(vec![1.0, 2.0])).render(dialect));
    session.run_ok(&Cell::closure_garbage("scaled").render(dialect));
    if Cell::closure_binding("callback").expressed_by(dialect) {
        session.run_ok(&Cell::closure_binding("callback").render(dialect));
    }
    let before = session.user_bindings();

    session.restart();
    assert_eq!(
        session.user_bindings(),
        before,
        "restoring must not change what the session holds"
    );

    // Different cells, none of which the pre-snapshot programs contained.
    session.run_ok(&Cell::extend("grown", "base", 3.0).render(dialect));
    let outcome = session.run_ok(&Cell::finish("grown").render(dialect));
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2, 3])));
}

/// Restarting between every pair of cells changes nothing a cell can see.
fn restarting_between_every_pair_of_cells_preserves_the_session(dialect: Dialect) {
    let cells = representative_session(dialect);
    let (resident, _) = drive(dialect, HarnessMode::Resident, &cells);
    let (restarting, _) = drive(dialect, HarnessMode::RestartBetweenCells, &cells);
    assert_eq!(
        resident.user_bindings(),
        restarting.user_bindings(),
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
fn a_restarted_session_persists_the_same_bytes(dialect: Dialect) {
    let cells = representative_session(dialect);
    let (resident, _) = drive(dialect, HarnessMode::Resident, &cells);
    let (restarting, _) = drive(dialect, HarnessMode::RestartBetweenCells, &cells);

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
fn a_snapshot_after_a_failing_cell_restores_cleanly(dialect: Dialect) {
    let (mut session, _) = drive(
        dialect,
        HarnessMode::Resident,
        &[
            Cell::bind("kept", Literal::List(vec![1.0, 2.0])),
            Cell::closure_garbage("scaled"),
            Cell::RuntimeError,
        ],
    );
    let before = session.user_bindings();
    session.restart();
    assert_eq!(session.user_bindings(), before);
    let outcome = session.run_ok(&Cell::finish("kept").render(dialect));
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2])));
}

/// Restoring twice in a row is the same as restoring once.
///
/// A rehydrating worker can lose its lease and hand the session on again before
/// running anything, so a restore has to be a fixed point.
fn restoring_twice_without_running_a_cell_is_a_fixed_point(dialect: Dialect) {
    let (mut session, _) = drive(
        dialect,
        HarnessMode::Resident,
        &representative_session(dialect),
    );
    let bindings = session.user_bindings();
    let persisted = session.persisted_state();
    session.restart();
    session.restart();
    session.restart();
    assert_eq!(session.user_bindings(), bindings);
    let after = session.persisted_state();
    assert_eq!(after.root, persisted.root);
    assert_eq!(after.components, persisted.components);
    let outcome = session.run_ok(&Cell::finish("grown").render(dialect));
    assert_eq!(outcome.finish, Some(serde_json::json!([1, 2, 3, 4])));
}

mod lashlang {
    use super::*;

    const DIALECT: Dialect = Dialect::Lashlang;

    #[test]
    fn a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells() {
        super::a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells(DIALECT);
    }

    #[test]
    fn restarting_between_every_pair_of_cells_preserves_the_session() {
        super::restarting_between_every_pair_of_cells_preserves_the_session(DIALECT);
    }

    #[test]
    fn a_restarted_session_persists_the_same_bytes() {
        super::a_restarted_session_persists_the_same_bytes(DIALECT);
    }

    #[test]
    fn a_snapshot_after_a_failing_cell_restores_cleanly() {
        super::a_snapshot_after_a_failing_cell_restores_cleanly(DIALECT);
    }

    #[test]
    fn restoring_twice_without_running_a_cell_is_a_fixed_point() {
        super::restoring_twice_without_running_a_cell_is_a_fixed_point(DIALECT);
    }
}

mod typescript {
    use super::*;

    const DIALECT: Dialect = Dialect::Typescript;

    #[test]
    fn a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells() {
        super::a_snapshot_after_closure_bearing_cells_restores_and_runs_different_cells(DIALECT);
    }

    #[test]
    fn restarting_between_every_pair_of_cells_preserves_the_session() {
        super::restarting_between_every_pair_of_cells_preserves_the_session(DIALECT);
    }

    #[test]
    fn a_restarted_session_persists_the_same_bytes() {
        super::a_restarted_session_persists_the_same_bytes(DIALECT);
    }

    #[test]
    fn a_snapshot_after_a_failing_cell_restores_cleanly() {
        super::a_snapshot_after_a_failing_cell_restores_cleanly(DIALECT);
    }

    #[test]
    fn restoring_twice_without_running_a_cell_is_a_fixed_point() {
        super::restoring_twice_without_running_a_cell_is_a_fixed_point(DIALECT);
    }
}
