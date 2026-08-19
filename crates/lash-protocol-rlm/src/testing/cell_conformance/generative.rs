//! Axis 5: randomized cell sequences against a model of the session.
//!
//! The hand-written scenarios cover the compositions someone thought of.
//! FIG-1562 was a composition nobody thought of — closure-bearing code plus a
//! second cell — so this axis generates sequences instead, and checks each one
//! against [`SessionModel`] rather than against itself. Every generated cell
//! must either succeed or fail typed, and after every cell the session's
//! bindings must equal the model's.
//!
//! The corpus is a fixture, not a random walk: fixed seeds, a fixed length, a
//! documented case count. Changing the generator changes the corpus, and
//! [`the_generated_corpus_is_deterministic`] makes that change deliberate. The
//! CI budget is spent here on breadth at short length; the `#[ignore]`d soak
//! runs the same generator far longer for anyone chasing a specific failure.

use super::drive;
use super::harness::{Dialect, HarnessMode};
use super::syntax::{Cell, Literal};

/// Sessions generated per dialect in the resident mode. Each session is eleven
/// cells — ten generated plus the terminal finish — so this is sixteen and a
/// half thousand cells per dialect, which the executor runs well inside the
/// suite's budget.
const RESIDENT_SESSIONS: u64 = 1_500;
/// Sessions generated per dialect in the restart-between-every-cell mode. Each
/// cell there costs a full capture and restore, so the count is smaller and the
/// breadth comes from the resident sweep above.
const RESTARTING_SESSIONS: u64 = 120;
/// Sessions per dialect in the `#[ignore]`d soak.
const SOAK_SESSIONS: u64 = 25_000;
/// Cells per generated session.
const SESSION_LENGTH: usize = 10;

/// SplitMix64. Small, seekable, and identical on every platform, which is what
/// a fixture corpus needs; the corpus must not depend on the host's `rand`.
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// What the generator knows a name currently holds, so it only emits cells the
/// dialect can actually run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    List,
    /// Bound to a record or to null: readable, but not a valid source for a
    /// derive or an extend.
    Opaque,
}

/// The names a generated session draws on. Small on purpose: a small pool means
/// shadowing, dropping, and re-binding the same name happen often, which is
/// where a stale root would show up.
const NAMES: &[&str] = &["alpha", "beta", "gamma", "delta"];

/// A name outside the pool, bound by the first cell and never touched again.
///
/// It exists so the terminal cell always has something live to finish with,
/// which keeps a generated session's length a function of the seed alone.
const ANCHOR: &str = "anchor";

/// Builds one session from `seed`.
///
/// The sequence always ends with a finish over a live name, so every generated
/// session also exercises the terminal path after whatever came before it.
pub(super) fn generate_session(dialect: Dialect, seed: u64) -> Vec<Cell> {
    let mut prng = Prng::new(seed);
    let mut kinds: std::collections::BTreeMap<&'static str, Kind> =
        std::collections::BTreeMap::new();
    let mut cells = Vec::with_capacity(SESSION_LENGTH + 1);

    // Every session starts bound, so the generator always has a source to read.
    cells.push(Cell::number(ANCHOR, 1.0));
    kinds.insert(ANCHOR, Kind::Number);

    while cells.len() < SESSION_LENGTH {
        let name = NAMES[prng.below(NAMES.len())];
        // A cell cannot read the name it binds — neither dialect can say that
        // — so the sources a derive or an extend may draw on exclude the
        // destination the generator already picked.
        let numbers = live_names(&kinds, Kind::Number, name);
        let lists = live_names(&kinds, Kind::List, name);
        let cell = match prng.below(12) {
            0 => {
                kinds.insert(name, Kind::Number);
                Cell::number(name, prng.below(50) as f64)
            }
            1 => {
                kinds.insert(name, Kind::List);
                Cell::bind(
                    name,
                    Literal::List((0..prng.below(4) + 1).map(|item| item as f64).collect()),
                )
            }
            2 => {
                kinds.insert(name, Kind::Opaque);
                Cell::bind(
                    name,
                    Literal::Nested {
                        scalar: prng.below(9) as f64,
                        items: vec![1.0, 2.0],
                    },
                )
            }
            3 if !numbers.is_empty() => {
                let source = numbers[prng.below(numbers.len())];
                kinds.insert(name, Kind::Number);
                Cell::derive(name, source)
            }
            4 if !lists.is_empty() => {
                let source = lists[prng.below(lists.len())];
                kinds.insert(name, Kind::List);
                Cell::extend(name, source, prng.below(20) as f64)
            }
            5 => {
                kinds.insert(name, Kind::Opaque);
                Cell::drop_value(name)
            }
            6 | 7 => {
                kinds.insert(name, Kind::Number);
                Cell::closure_garbage(name)
            }
            8 if Cell::closure_binding(name).expressed_by(dialect) => {
                kinds.remove(name);
                Cell::closure_binding(name)
            }
            9 => Cell::CompileError,
            10 => Cell::RuntimeError,
            11 if Cell::Refusal.expressed_by(dialect) => Cell::Refusal,
            // The guarded arms above fall through here when the dialect or the
            // session state cannot supply them, which keeps the sequence length
            // a function of the seed alone rather than of the dialect.
            _ => {
                kinds.insert(name, Kind::Number);
                Cell::closure_garbage(name)
            }
        };
        cells.push(cell);
    }

    let terminal = live_names(&kinds, Kind::Number, "")
        .first()
        .copied()
        .expect("the anchor keeps a live number in every generated session");
    cells.push(Cell::finish(terminal));
    cells
}

fn live_names(
    kinds: &std::collections::BTreeMap<&'static str, Kind>,
    kind: Kind,
    excluding: &str,
) -> Vec<&'static str> {
    kinds
        .iter()
        .filter(|(name, held)| **held == kind && **name != excluding)
        .map(|(name, _)| *name)
        .collect()
}

fn sweep(dialect: Dialect, mode: HarnessMode, sessions: u64) {
    for seed in 0..sessions {
        let cells = generate_session(dialect, seed);
        drive(dialect, mode, &cells);
    }
}

mod lashlang {
    use super::*;

    const DIALECT: Dialect = Dialect::Lashlang;

    #[test]
    fn generated_sessions_never_poison_a_session() {
        sweep(DIALECT, HarnessMode::Resident, RESIDENT_SESSIONS);
    }

    #[test]
    fn generated_sessions_survive_a_restart_between_every_cell() {
        sweep(
            DIALECT,
            HarnessMode::RestartBetweenCells,
            RESTARTING_SESSIONS,
        );
    }

    #[test]
    #[ignore = "soak: the same generator, far longer; run it when chasing a generated failure"]
    fn generated_sessions_soak() {
        sweep(DIALECT, HarnessMode::Resident, SOAK_SESSIONS);
    }
}

mod typescript {
    use super::*;

    const DIALECT: Dialect = Dialect::Typescript;

    #[test]
    fn generated_sessions_never_poison_a_session() {
        sweep(DIALECT, HarnessMode::Resident, RESIDENT_SESSIONS);
    }

    #[test]
    fn generated_sessions_survive_a_restart_between_every_cell() {
        sweep(
            DIALECT,
            HarnessMode::RestartBetweenCells,
            RESTARTING_SESSIONS,
        );
    }

    #[test]
    #[ignore = "soak: the same generator, far longer; run it when chasing a generated failure"]
    fn generated_sessions_soak() {
        sweep(DIALECT, HarnessMode::Resident, SOAK_SESSIONS);
    }
}

/// The corpus is a fixture: the same seed builds the same session, different
/// seeds build different ones, and the length does not depend on the dialect.
#[test]
fn the_generated_corpus_is_deterministic() {
    for dialect in Dialect::ALL {
        let first = generate_session(*dialect, 7);
        assert_eq!(first, generate_session(*dialect, 7), "{dialect}");
        assert_ne!(first, generate_session(*dialect, 8), "{dialect}");
        assert_eq!(first.len(), SESSION_LENGTH + 1, "{dialect}");
    }
    assert_ne!(
        generate_session(Dialect::Lashlang, 3),
        generate_session(Dialect::Typescript, 3),
        "the dialects differ in the cells they can express, so their corpora differ"
    );
}

/// The corpus reaches every cell shape it is supposed to.
///
/// A generator that never emits a closure cell would pass the sweep above
/// without testing anything, which is the failure mode of every property test
/// that only asserts an invariant.
#[test]
fn the_generated_corpus_reaches_every_cell_shape() {
    for dialect in Dialect::ALL {
        let mut seen_closure_garbage = false;
        let mut seen_closure_binding = false;
        let mut seen_failure = false;
        let mut seen_extend = false;
        let mut seen_drop = false;
        for seed in 0..RESIDENT_SESSIONS {
            for cell in generate_session(*dialect, seed) {
                match cell {
                    Cell::ClosureGarbage { .. } => seen_closure_garbage = true,
                    Cell::ClosureBinding { .. } => seen_closure_binding = true,
                    Cell::CompileError | Cell::RuntimeError | Cell::Refusal => seen_failure = true,
                    Cell::Extend { .. } => seen_extend = true,
                    Cell::Drop { .. } => seen_drop = true,
                    _ => {}
                }
            }
        }
        assert!(seen_closure_garbage, "{dialect}: no closure-garbage cell");
        assert!(seen_failure, "{dialect}: no failing cell");
        assert!(seen_extend, "{dialect}: no extend cell");
        assert!(seen_drop, "{dialect}: no drop cell");
        assert_eq!(
            seen_closure_binding,
            *dialect == Dialect::Typescript,
            "{dialect}: closure-valued bindings appear exactly where the dialect expresses them"
        );
    }
}
