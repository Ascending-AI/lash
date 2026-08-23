//! Multi-cell and heap conformance for the RLM dialects.
//!
//! An RLM session is a *sequence* of cells: each one compiles its own
//! `CompiledProgram` while the execution state — heap included — carries over.
//! Every ingredient of that had coverage before FIG-1562 and the composition
//! had none, so a closure allocated by one cell was re-validated against the
//! next cell's function table and poisoned the session for everything after it,
//! down to `finish(6 * 7);`. A downstream host found it in production.
//!
//! This suite covers the composition, in both dialects, along six axes, one
//! module each:
//!
//! * [`multi_cell`] — sequences that create, read, shadow, grow, and drop real
//!   values across cells, with failing cells interleaved.
//! * [`gc_boundary`] — what the collection at the cell boundary must remove
//!   (the previous cell's garbage), what it must keep (everything rooted), and
//!   that a long session's persisted state does not grow without bound.
//! * [`no_poisoning`] — the FIG-1562 law stated generally: a cell that fails,
//!   however it fails, leaves the session coherent for the next one.
//! * [`persistence`] — the same laws across snapshot and restore, including a
//!   harness mode that restarts between every pair of cells.
//! * [`generative`] — randomized cell sequences checked against a model of
//!   what the session should hold, with a fixed seed and a bounded budget.
//! * [`parity`] — where both dialects express a scenario they must agree, and
//!   where one cannot, the matrix says so with a reason.
//!
//! Every scenario is its own `#[test]`, per dialect, so `nextest` shards them
//! and a failure names one cell sequence rather than a bundle. A whole dialect
//! runs with a filter on its module path, for example
//! `-E 'test(/cell_conformance::.*::typescript::/)'`.

mod gc_boundary;
mod generative;
mod harness;
mod multi_cell;
mod no_poisoning;
mod parity;
mod parked_continuation;
mod persistence;
mod syntax;

use harness::{CellOutcome, Dialect, HarnessMode, Session};
use syntax::{Cell, Expectation, SessionModel};

/// Fragments of the diagnostics a cell inherits from an earlier cell's program.
///
/// No cell may ever produce one of these: a closure index, a capture count, or
/// a function table are facts about the program that compiled the closure, and
/// a later cell's program makes no claim about them.
///
/// This lives here rather than in [`no_poisoning`] because it has to run on
/// *every* outcome the suite produces, not only the ones a no-poisoning
/// scenario looks at. A cell that fails for its own reason is still forbidden
/// to fail for an earlier cell's reason at the same time, and a stale closure
/// index that only surfaces while compiling a cell which also has a defect of
/// its own would otherwise slide through the whole generative sweep behind a
/// bare "it failed, as expected".
const INHERITED_DIAGNOSTICS: &[&str] = &[
    "is not present in the compiled program",
    "UnknownFunction",
    "ClosureCaptureCountMismatch",
    "closure function index",
];

/// Asserts an outcome is not one cell blaming another.
fn assert_not_inherited(outcome: &CellOutcome, context: &str) {
    let Some(error) = outcome.error.as_deref() else {
        return;
    };
    for fragment in INHERITED_DIAGNOSTICS {
        assert!(
            !error.contains(fragment),
            "{context}: a cell failed with a diagnostic about an earlier cell's program \
             (`{fragment}`): {error}"
        );
    }
}

/// Runs `cells` as one session and checks every cell against the model.
///
/// This is the suite's workhorse, and it asserts more than the scenario that
/// called it usually names. After *every* cell: the session's bindings equal
/// the model's; the cell either succeeded or failed typed; the terminal value
/// is exactly the one the model says it is, present only where a finish cell
/// put it; and no outcome — succeeding or failing — carries a diagnostic about
/// an earlier cell's program. A scenario therefore never has to restate the
/// cross-cell laws it is not about, and a sequence written for one axis catches
/// a violation of another.
fn drive(dialect: Dialect, mode: HarnessMode, cells: &[Cell]) -> (Session, SessionModel) {
    let mut session = Session::open(dialect, mode);
    let mut model = SessionModel::new();
    for (index, cell) in cells.iter().enumerate() {
        assert!(
            cell.expressed_by(dialect),
            "cell {index} ({cell:?}) is not expressed by {dialect}"
        );
        let source = cell.render(dialect);
        let expectation = model.apply(cell);
        let outcome = session.run(&source);
        let context = format!("cell {index} of the {dialect} session ({source:?})");
        assert_not_inherited(&outcome, &context);
        match expectation {
            Expectation::Succeeds { finish } => {
                assert!(outcome.succeeded(), "{context} failed: {:?}", outcome.error);
                assert_eq!(
                    outcome.finish, finish,
                    "{context} produced the wrong terminal value"
                );
            }
            Expectation::FailsTyped => {
                let failure = outcome.error.as_deref().unwrap_or("");
                assert!(
                    !failure.is_empty(),
                    "{context} was required to fail typed, and reported {outcome:?}"
                );
                assert_eq!(
                    outcome.finish, None,
                    "{context} failed and still finished the session"
                );
            }
        }
        assert_eq!(
            &session.user_bindings(),
            model.bindings(),
            "after {context} the {dialect} session's bindings left the model"
        );
    }
    (session, model)
}
