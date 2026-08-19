//! Axis 6: where both dialects express a scenario, they must agree.
//!
//! Both dialects compile to the same VM and share one execution state, so a
//! cross-cell law that held in one and not the other would mean the law lives
//! in a front end rather than in the session. The rows below are the matrix:
//! each names a scenario and says, per dialect, whether the dialect can express
//! it — and when it cannot, why. Deterministic tier throughout; nothing here
//! calls a model.
//!
//! A "not expressed" row is a claim about a dialect, so each one has a test
//! that proves the claim rather than a comment asserting it.

use super::harness::{Dialect, HarnessMode, Session};
use super::syntax::{Cell, Literal};

/// Whether a dialect can express a scenario, and why not when it cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Expressed,
    NotExpressed(&'static str),
}

struct ParityRow {
    scenario: &'static str,
    lashlang: Disposition,
    typescript: Disposition,
}

const PARITY_MATRIX: &[ParityRow] = &[
    ParityRow {
        scenario: "a binding survives across cells",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a failing cell preserves the session",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a garbage-producing cell does not poison the next",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a structure grows across cells",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a restart between every cell preserves the session",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "the persisted state is stable across repeated cells",
        lashlang: Disposition::Expressed,
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a closure-valued binding is dropped at the boundary",
        lashlang: Disposition::NotExpressed(
            "Lashlang has no first-class function value: `fn` is a declaration, not an \
             expression, so no cell can bind a name to a closure",
        ),
        typescript: Disposition::Expressed,
    },
    ParityRow {
        scenario: "a refusal is a distinct failure tier from a wrong program",
        lashlang: Disposition::NotExpressed(
            "Lashlang reports a construct outside the language as an unknown name, the same \
             tier as a misspelling, so there is no separate refusal to compose with",
        ),
        typescript: Disposition::Expressed,
    },
];

/// What a scenario looks like from outside, in terms both dialects share.
///
/// Persisted *sizes* are deliberately absent: the two dialects encode different
/// bindings for the same scenario, so comparing bytes would compare encodings.
/// What must match is the shape of the session's behaviour.
#[derive(Debug, PartialEq)]
struct Observation {
    /// Whether each cell succeeded, in order.
    cells_succeeded: Vec<bool>,
    /// The session's bindings at the end.
    bindings: std::collections::BTreeMap<String, serde_json::Value>,
    /// The terminal value, if the last cell finished.
    finish: Option<serde_json::Value>,
}

fn observe(dialect: Dialect, mode: HarnessMode, cells: &[Cell]) -> Observation {
    let mut session = Session::open(dialect, mode);
    let mut cells_succeeded = Vec::with_capacity(cells.len());
    let mut finish = None;
    for cell in cells {
        let outcome = session.run(&cell.render(dialect));
        cells_succeeded.push(outcome.succeeded());
        if outcome.finish.is_some() {
            finish = outcome.finish;
        }
    }
    Observation {
        cells_succeeded,
        bindings: session.user_bindings(),
        finish,
    }
}

fn assert_dialects_agree(mode: HarnessMode, cells: &[Cell]) {
    let lashlang = observe(Dialect::Lashlang, mode, cells);
    let typescript = observe(Dialect::Typescript, mode, cells);
    assert_eq!(
        lashlang, typescript,
        "the dialects disagreed on a scenario both express, in {mode:?}"
    );
}

#[test]
fn both_dialects_agree_that_a_binding_survives_across_cells() {
    assert_dialects_agree(
        HarnessMode::Resident,
        &[
            Cell::number("seed", 41.0),
            Cell::bind("noise", Literal::List(vec![1.0, 2.0])),
            Cell::derive("answer", "seed"),
            Cell::finish("answer"),
        ],
    );
}

#[test]
fn both_dialects_agree_that_a_failing_cell_preserves_the_session() {
    assert_dialects_agree(
        HarnessMode::Resident,
        &[
            Cell::bind("kept", Literal::List(vec![1.0, 2.0])),
            Cell::CompileError,
            Cell::RuntimeError,
            Cell::extend("grown", "kept", 3.0),
            Cell::finish("grown"),
        ],
    );
}

#[test]
fn both_dialects_agree_that_a_garbage_producing_cell_does_not_poison_the_next() {
    assert_dialects_agree(
        HarnessMode::Resident,
        &[
            Cell::closure_garbage("scaled"),
            Cell::closure_garbage("scaled_again"),
            Cell::number("answer", 42.0),
            Cell::finish("answer"),
        ],
    );
}

#[test]
fn both_dialects_agree_that_a_structure_grows_across_cells() {
    assert_dialects_agree(
        HarnessMode::Resident,
        &[
            Cell::bind("base", Literal::List(vec![1.0])),
            Cell::extend("second", "base", 2.0),
            Cell::extend("third", "second", 3.0),
            Cell::finish("third"),
        ],
    );
}

#[test]
fn both_dialects_agree_when_the_session_restarts_between_every_cell() {
    assert_dialects_agree(
        HarnessMode::RestartBetweenCells,
        &[
            Cell::bind("base", Literal::List(vec![1.0, 2.0])),
            Cell::closure_garbage("scaled"),
            Cell::RuntimeError,
            Cell::extend("grown", "base", 3.0),
            Cell::drop_value("base"),
            Cell::finish("grown"),
        ],
    );
}

/// Both dialects hold their persisted state steady over a repeated cell.
///
/// The sizes themselves differ — different bindings, different encodings — so
/// what is compared is the property: neither dialect grows.
#[test]
fn both_dialects_agree_that_the_persisted_state_is_stable_across_repeated_cells() {
    for dialect in Dialect::ALL {
        let mut session = Session::open(*dialect, HarnessMode::Resident);
        let mut sizes = Vec::new();
        for _ in 0..12 {
            session.run_ok(&Cell::closure_garbage("scaled").render(*dialect));
            sizes.push(session.persisted_bytes());
        }
        assert!(
            sizes.iter().all(|size| *size == sizes[0]),
            "{dialect} grew its persisted state over a repeated cell: {sizes:?}"
        );
    }
}

/// The first "not expressed" claim, proved rather than asserted: Lashlang has
/// no way to bind a name to a function.
#[test]
fn lashlang_cannot_bind_a_function_value_to_a_name() {
    let mut session = Session::open(Dialect::Lashlang, HarnessMode::Resident);
    let failure = session.run_failing("scale = fn (n: float) -> float { n * 2 }");
    assert!(
        failure.contains("expected"),
        "the rejection should be a parse error: {failure}"
    );
    // And the session is unharmed, which is the same law as everywhere else.
    assert!(session.run("finish 6 * 7").succeeded());
}

/// The second: an out-of-language construct in Lashlang arrives in the same
/// tier as a misspelling, so there is no separate refusal to compose with.
#[test]
fn lashlang_reports_an_out_of_language_construct_in_the_ordinary_error_tier() {
    let mut lashlang = Session::open(Dialect::Lashlang, HarnessMode::Resident);
    let lashlang_failure = lashlang.run_failing("class NotADialectConstruct {}");
    assert!(
        lashlang_failure.starts_with("[ERROR]"),
        "Lashlang has no refusal tier: {lashlang_failure}"
    );

    let mut typescript = Session::open(Dialect::Typescript, HarnessMode::Resident);
    let typescript_failure = typescript.run_failing(&Cell::Refusal.render(Dialect::Typescript));
    assert!(
        typescript_failure.starts_with("[POLICY]"),
        "TypeScript separates a refusal from a wrong program: {typescript_failure}"
    );
}

/// Every row that a dialect does not express says why, and no row is expressed
/// by neither dialect.
#[test]
fn the_parity_matrix_explains_every_gap() {
    for row in PARITY_MATRIX {
        for disposition in [row.lashlang, row.typescript] {
            if let Disposition::NotExpressed(reason) = disposition {
                assert!(
                    reason.len() > 40,
                    "row `{}` records a gap without explaining it: {reason:?}",
                    row.scenario
                );
            }
        }
        assert!(
            row.lashlang == Disposition::Expressed || row.typescript == Disposition::Expressed,
            "row `{}` is expressed by neither dialect, so it is not a parity row",
            row.scenario
        );
    }
}
