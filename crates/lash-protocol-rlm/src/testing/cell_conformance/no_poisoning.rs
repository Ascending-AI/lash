//! Axis 3: the FIG-1562 law, stated generally.
//!
//! *A cell that fails — compile error, runtime error, dialect refusal — must
//! never corrupt the session for the cells after it, and no cell may fail for
//! something an earlier cell did.* Every cell either runs or fails typed, and
//! the next cell starts from a coherent heap.
//!
//! One honest limit, recorded so nobody reads more into a green run than is
//! there. Through an RLM *Lashlang* cell the closure half of the law is sampled
//! rather than asserted: a declared `fn` does leave a capture-free closure on
//! the heap, but whether a stale function index poisons the next cell depends on
//! what that cell's chunk happens to contain, and the trivial cells here happen
//! to contain something it lands on. Against the pre-fix revision the whole
//! Lashlang side of this suite therefore stays green while the TypeScript side
//! goes red. The Lashlang closure law is asserted where it can be —
//! `lashlang`'s `tests/functions.rs`, against a next program with no functions
//! at all — and that test is red before the fix.
//!
//! The second half is the one that shipped broken. The reported failure was not
//! "the session is gone" but a *later* cell failing with `closure function
//! index 0 is not present in the compiled program` — a diagnostic about the
//! previous cell's program, raised against this cell's. Every scenario here
//! therefore checks not only that the next cell succeeds but that when a cell
//! does fail, it fails for its own reason.

use super::harness::{Dialect, HarnessMode, Session};
use super::syntax::{Cell, Literal};
use super::{assert_not_inherited, drive};

// The inherited-diagnostic check this axis is named for lives in the suite root
// and runs inside `drive` on every outcome of every scenario. The scenarios here
// call it directly for the sequences they build cell by cell rather than through
// `drive`.

/// The trivial cell from the FIG-1562 report. A session that cannot run this
/// cannot run anything.
const TRIVIAL_CELL: &str = "finish 6 * 7";
const TRIVIAL_CELL_TYPESCRIPT: &str = "finish(6 * 7);";

fn trivial_cell(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Lashlang => TRIVIAL_CELL,
        Dialect::Typescript => TRIVIAL_CELL_TYPESCRIPT,
    }
}

/// Closure-bearing cells, one per shape a closure reaches the boundary in.
///
/// The TypeScript list mirrors `lash-typescript`'s durability corpus: a
/// recursive function, a nested function, an arrow that captures and is
/// returned, an inline arrow whose closure is garbage at once, a closure bound
/// straight to a global, and a closure inside a container. Lashlang has no
/// first-class function value, but a declared `fn` is materialized at its call
/// site as a capture-free closure over the chunk function, so its declarations
/// leave the same garbage behind.
fn closure_bearing_cell(dialect: Dialect, shape: ClosureShape) -> &'static str {
    match (dialect, shape) {
        (Dialect::Lashlang, ClosureShape::Recursive) => {
            "fn fact(n: float) -> float { if n <= 1 { 1 } else { fact(n - 1) * n } }\nf5 = fact(5)"
        }
        (Dialect::Lashlang, ClosureShape::Plain) => {
            "fn scale(n: float) -> float { n * 2 }\nscaled = scale(21)"
        }
        (Dialect::Lashlang, ClosureShape::Nested) => {
            "fn inner(n: float) -> float { n + 1 }\nfn outer(n: float) -> float { inner(n) * 2 }\nnested = outer(4)"
        }
        (Dialect::Typescript, ClosureShape::Recursive) => {
            "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; }\nconst f5 = fact(5);"
        }
        (Dialect::Typescript, ClosureShape::Plain) => "const xs = [1].map(value => value + 1);",
        (Dialect::Typescript, ClosureShape::Nested) => {
            "const top = 9;\nfunction outerFn(): number { function innerFn(): number { return top; } return innerFn(); }\nconst nested = outerFn();"
        }
        (Dialect::Typescript, ClosureShape::Returned) => {
            "const base = 10;\nconst outer = () => { const inner = () => base; return inner; };\nconst held = outer();"
        }
        (Dialect::Typescript, ClosureShape::BoundToGlobal) => {
            "const add = (value: number) => value + 1;\nconst y = add(1);"
        }
        (Dialect::Typescript, ClosureShape::InsideContainer) => {
            "const handlers = { onDone: (value: number) => value + 1 };\nconst tag = \"kept\";"
        }
        (Dialect::Lashlang, shape) => {
            unreachable!("Lashlang does not express {shape:?}; see the parity matrix")
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ClosureShape {
    Recursive,
    Plain,
    Nested,
    Returned,
    BoundToGlobal,
    InsideContainer,
}

/// One closure shape, then a trivial cell. The reported bug, per shape.
fn a_closure_bearing_cell_does_not_poison_the_next_cell(dialect: Dialect, shape: ClosureShape) {
    for mode in HarnessMode::ALL {
        let mut session = Session::open(dialect, *mode);
        let first = session.run(closure_bearing_cell(dialect, shape));
        assert!(
            first.succeeded(),
            "the {shape:?} cell itself failed in {dialect}/{mode:?}: {:?}",
            first.error
        );
        let second = session.run(trivial_cell(dialect));
        assert_not_inherited(&second, &format!("{dialect}/{mode:?} after {shape:?}"));
        assert!(
            second.succeeded(),
            "the trivial cell after the {shape:?} cell failed in {dialect}/{mode:?}: {:?}",
            second.error
        );
        assert_eq!(second.finish, Some(serde_json::json!(42)));
    }
}

/// A cell that fails to compile leaves nothing behind.
fn a_compile_error_does_not_poison_the_session(dialect: Dialect) {
    let (mut session, _) = drive(
        dialect,
        HarnessMode::Resident,
        &[
            Cell::bind("before", Literal::List(vec![1.0, 2.0])),
            Cell::CompileError,
        ],
    );
    let outcome = session.run(trivial_cell(dialect));
    assert_not_inherited(&outcome, "after a compile error");
    assert!(outcome.succeeded(), "{:?}", outcome.error);
    assert_eq!(
        session.user_bindings().get("before"),
        Some(&serde_json::json!([1, 2]))
    );
}

/// A cell that fails at runtime leaves nothing behind either.
fn a_runtime_error_does_not_poison_the_session(dialect: Dialect) {
    let (mut session, _) = drive(
        dialect,
        HarnessMode::Resident,
        &[
            Cell::bind("before", Literal::List(vec![1.0, 2.0])),
            Cell::RuntimeError,
        ],
    );
    let outcome = session.run(trivial_cell(dialect));
    assert_not_inherited(&outcome, "after a runtime error");
    assert!(outcome.succeeded(), "{:?}", outcome.error);
    assert_eq!(
        session.user_bindings().get("before"),
        Some(&serde_json::json!([1, 2]))
    );
}

/// A refusal is a different failure tier from a wrong program, and it must be
/// just as harmless. TypeScript only; Lashlang has no refusal tier.
fn a_refusal_does_not_poison_the_session(dialect: Dialect) {
    let (mut session, _) = drive(
        dialect,
        HarnessMode::Resident,
        &[
            Cell::bind("before", Literal::List(vec![1.0, 2.0])),
            Cell::Refusal,
        ],
    );
    let outcome = session.run(trivial_cell(dialect));
    assert_not_inherited(&outcome, "after a refusal");
    assert!(outcome.succeeded(), "{:?}", outcome.error);
}

/// A failing cell that follows a closure-bearing cell fails for its own reason.
///
/// This is the diagnostic-honesty half of the law, and it is the one that makes
/// the suite red on the unfixed bug for a *failing* cell as well as a working
/// one: before the fix, the second cell's error named a function index from the
/// first cell's program instead of its own defect.
fn a_failing_cell_after_a_closure_cell_fails_for_its_own_reason(dialect: Dialect) {
    let mut session = Session::open(dialect, HarnessMode::Resident);
    session.run_ok(closure_bearing_cell(dialect, ClosureShape::Plain));
    let failing = Cell::CompileError.render(dialect);
    let outcome = session.run(&failing);
    assert_not_inherited(&outcome, "a failing cell after a closure cell");
    assert!(
        outcome.failure().contains("no_such_name") || outcome.failure().contains("noSuchName"),
        "the failure must name the cell's own defect: {}",
        outcome.failure()
    );
}

/// A run of failing cells is still just a run of failures.
fn a_run_of_failing_cells_does_not_poison_the_session(dialect: Dialect) {
    let mut cells = vec![Cell::bind("kept", Literal::List(vec![1.0, 2.0, 3.0]))];
    for _ in 0..4 {
        cells.push(Cell::CompileError);
        cells.push(Cell::RuntimeError);
    }
    cells.push(Cell::closure_garbage("scaled"));
    cells.push(Cell::extend("grown", "kept", 4.0));

    let (session, _) = drive(dialect, HarnessMode::Resident, &cells);
    assert_eq!(
        session.user_bindings().get("grown"),
        Some(&serde_json::json!([1, 2, 3, 4]))
    );
}

mod lashlang {
    use super::*;

    const DIALECT: Dialect = Dialect::Lashlang;

    #[test]
    fn a_recursive_function_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(
            DIALECT,
            ClosureShape::Recursive,
        );
    }

    #[test]
    fn a_declared_function_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(DIALECT, ClosureShape::Plain);
    }

    #[test]
    fn a_nested_call_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(DIALECT, ClosureShape::Nested);
    }

    #[test]
    fn a_compile_error_does_not_poison_the_session() {
        super::a_compile_error_does_not_poison_the_session(DIALECT);
    }

    #[test]
    fn a_runtime_error_does_not_poison_the_session() {
        super::a_runtime_error_does_not_poison_the_session(DIALECT);
    }

    #[test]
    fn a_failing_cell_after_a_closure_cell_fails_for_its_own_reason() {
        super::a_failing_cell_after_a_closure_cell_fails_for_its_own_reason(DIALECT);
    }

    #[test]
    fn a_run_of_failing_cells_does_not_poison_the_session() {
        super::a_run_of_failing_cells_does_not_poison_the_session(DIALECT);
    }
}

mod typescript {
    use super::*;

    const DIALECT: Dialect = Dialect::Typescript;

    #[test]
    fn a_recursive_function_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(
            DIALECT,
            ClosureShape::Recursive,
        );
    }

    #[test]
    fn an_inline_arrow_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(DIALECT, ClosureShape::Plain);
    }

    #[test]
    fn a_nested_function_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(DIALECT, ClosureShape::Nested);
    }

    #[test]
    fn a_returned_closure_cell_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(
            DIALECT,
            ClosureShape::Returned,
        );
    }

    #[test]
    fn a_closure_bound_to_a_global_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(
            DIALECT,
            ClosureShape::BoundToGlobal,
        );
    }

    #[test]
    fn a_closure_inside_a_container_does_not_poison_the_next_cell() {
        super::a_closure_bearing_cell_does_not_poison_the_next_cell(
            DIALECT,
            ClosureShape::InsideContainer,
        );
    }

    #[test]
    fn a_compile_error_does_not_poison_the_session() {
        super::a_compile_error_does_not_poison_the_session(DIALECT);
    }

    #[test]
    fn a_runtime_error_does_not_poison_the_session() {
        super::a_runtime_error_does_not_poison_the_session(DIALECT);
    }

    #[test]
    fn a_refusal_does_not_poison_the_session() {
        super::a_refusal_does_not_poison_the_session(DIALECT);
    }

    #[test]
    fn a_failing_cell_after_a_closure_cell_fails_for_its_own_reason() {
        super::a_failing_cell_after_a_closure_cell_fails_for_its_own_reason(DIALECT);
    }

    #[test]
    fn a_run_of_failing_cells_does_not_poison_the_session() {
        super::a_run_of_failing_cells_does_not_poison_the_session(DIALECT);
    }
}
