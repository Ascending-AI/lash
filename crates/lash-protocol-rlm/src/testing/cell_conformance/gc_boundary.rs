//! Axis 2: what the collection at the cell boundary removes, and what it keeps.
//!
//! The heap outlives the program that filled it, so cell N's garbage is still
//! resident when cell N+1's program is validated. The boundary collects against
//! the live roots first, which is what makes that garbage invisible to the next
//! cell — and the same collection must not touch anything a root still reaches,
//! or a session would lose a binding to an implementation detail.
//!
//! The leak regression is stated over the *persisted* state rather than over
//! heap internals. An unbounded heap that never reaches the wire costs a host
//! nothing; a bounded heap that writes an unbounded snapshot costs it a growing
//! write on every turn. The persisted size is the number a host pays for.

use super::drive;
use super::harness::{HarnessMode, Session};
use super::syntax::{Cell, Literal};

/// The FIG-1562 law at its narrowest: a cell that leaves a closure behind as
/// garbage must not fail the next cell.
#[test]
fn garbage_from_one_cell_does_not_reach_the_next_cells_validation() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok(&Cell::closure_garbage("scaled").render());
    let outcome = session.run_ok(&Cell::number("answer", 42.0).render());
    assert!(outcome.succeeded());
    assert_eq!(
        session.user_bindings().get("scaled"),
        Some(&serde_json::json!(6)),
        "the value the garbage-producing cell computed still belongs to the session"
    );
}

/// Garbage left behind by a cell that *failed* is garbage too.
#[test]
fn garbage_from_a_failed_cell_does_not_reach_the_next_cell() {
    let (session, _) = drive(
        HarnessMode::Resident,
        &[
            Cell::closure_garbage("scaled"),
            Cell::RuntimeError,
            Cell::closure_garbage("scaled_again"),
            Cell::number("answer", 42.0),
        ],
    );
    assert_eq!(
        session.user_bindings().get("answer"),
        Some(&serde_json::json!(42))
    );
}

/// A structure a root still reaches survives every boundary collection.
#[test]
fn a_rooted_structure_survives_the_boundary_collection() {
    let payload = (0..24).map(f64::from).collect::<Vec<_>>();
    let mut cells = vec![Cell::bind("rooted", Literal::List(payload.clone()))];
    for step in 0..6 {
        cells.push(Cell::closure_garbage(&format!("garbage{step}")));
    }
    let (session, _) = drive(HarnessMode::Resident, &cells);
    assert_eq!(
        session.user_bindings().get("rooted"),
        Some(
            &payload
                .iter()
                .map(|v| *v as i64)
                .collect::<serde_json::Value>()
        ),
        "six collections must not disturb a rooted list"
    );
}

/// A long session of garbage-producing cells does not grow what it persists.
///
/// The cells rebind the same names every time, so the session's live data is
/// constant by construction and any growth is the heap or the encoder keeping
/// something it should have dropped. Thirty cells is enough for a per-cell leak
/// to be unmistakable and short enough to stay inside the CI budget.
fn many_garbage_producing_cells_do_not_grow_the_persisted_state(mode: HarnessMode) {
    const CELLS: usize = 30;
    // The first cells introduce the bindings, so the size only means anything
    // once every name exists.
    const WARMUP: usize = 3;

    let mut session = Session::open(mode);
    let mut sizes = Vec::with_capacity(CELLS);
    for step in 0..CELLS {
        session.run_ok(&Cell::closure_garbage("scaled").render());
        session.run_ok(
            &Cell::bind(
                "scratch",
                Literal::List(vec![step as f64, step as f64 + 1.0, step as f64 + 2.0]),
            )
            .render(),
        );
        sizes.push(session.persisted_bytes());
    }

    let settled = sizes[WARMUP];
    assert!(
        sizes[WARMUP..].iter().all(|size| *size == settled),
        "the persisted state of a constant-data session must not move after it settles: {sizes:?}"
    );
}

/// Alternating cell shapes must not retain each lowering's generated
/// TypeScript bindings.
///
/// The low-temporary cell leaves the previous high-temporary cell's scratch
/// names untouched, while the high-temporary cell exercises several lowering
/// temporaries in one shape. Sampling only after each high cell makes the
/// contract explicit: the same live data and lowering shape must produce the
/// same persisted state throughout the session.
#[test]
fn alternating_low_and_high_temporary_cells_do_not_grow_the_persisted_state() {
    const ROUNDS: usize = 12;
    const WARMUP: usize = 3;
    // Raw TypeScript is required because no Cell variant expresses this repeated-filter lowering shape.
    const HIGH_TEMPORARY_CELL: &str = "const high = [1,2,3].filter(value=>value>=0).filter(value=>value>=0).filter(value=>value>=0).filter(value=>value>=0);";

    let mut session = Session::open(HarnessMode::Resident);
    let mut high_sizes = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        session.run_ok("const low = 1;");
        session.run_ok(&format!("{HIGH_TEMPORARY_CELL} // round {round}"));
        high_sizes.push(session.persisted_bytes());
    }

    let settled = high_sizes[WARMUP];
    assert!(
        high_sizes[WARMUP..].iter().all(|size| *size == settled),
        "alternating low/high temporary cells grew persisted state: {high_sizes:?}"
    );
}

/// Dropping a binding gives its bytes back.
///
/// The complement of the leak regression: a session that never shrinks is a
/// leak with extra steps, so the boundary has to actually reclaim.
#[test]
fn dropping_a_large_binding_shrinks_the_persisted_state() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok(&Cell::number("anchor", 1.0).render());
    let empty = session.persisted_bytes();

    let payload = (0..64).map(f64::from).collect::<Vec<_>>();
    session.run_ok(&Cell::bind("payload", Literal::List(payload)).render());
    let loaded = session.persisted_bytes();
    assert!(
        loaded > empty,
        "a 64-element list must cost something to persist: {empty} -> {loaded}"
    );

    session.run_ok(&Cell::drop_value("payload").render());
    let dropped = session.persisted_bytes();
    assert!(
        dropped < loaded,
        "dropping the list must give its bytes back: {loaded} -> {dropped}"
    );
}

/// A closure-valued binding is gone from the runtime roots, not merely hidden
/// from the exported view.
///
/// The two used to disagree: the host-view projection dropped the binding
/// from what a host and a model see while the runtime roots still reached the
/// closure, so collection could not remove it and the next cell's validation
/// judged it. The observable form of "the roots match the view" is that the
/// next cell does not know the name at all.
#[test]
fn a_closure_valued_name_is_unknown_to_the_next_cell() {
    let mut session = Session::open(HarnessMode::Resident);
    session.run_ok(&Cell::closure_binding("callback").render());
    let failure = session.run_failing("finish(callback(1));");
    assert!(
        failure.contains("callback"),
        "the next cell must be told the name is not bound: {failure}"
    );
    // And the session is still usable afterwards, which is the whole point.
    assert_eq!(
        session.run_ok("finish(6 * 7);").finish,
        Some(serde_json::json!(42))
    );
}

#[test]
fn many_garbage_producing_cells_do_not_grow_the_persisted_state_resident() {
    many_garbage_producing_cells_do_not_grow_the_persisted_state(HarnessMode::Resident);
}

#[test]
fn many_garbage_producing_cells_do_not_grow_the_persisted_state_across_restarts() {
    many_garbage_producing_cells_do_not_grow_the_persisted_state(HarnessMode::RestartBetweenCells);
}
