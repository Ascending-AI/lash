//! The r56 cold probes and the pressure rows on the bytecode core
//! (law-inventory `laws/cold`; cutover contract §6), each a lowered
//! Python program whose literal prints and end are CPython's output of
//! its source.
//!
//! - COLD-PROBE: a run sealed and opened cold at every slice return, in a
//!   machine built from a program linked afresh, prints and ends as the
//!   literal says, at every slice; and is charged what the run that never
//!   paused is charged. The probes hold shared rows, a cycle, rows edited
//!   in place through aliases, and texts and maps reached twice, so each
//!   reopen must restore one object per identity.
//! - PRESSURE: under the least memory bound the unpaused run finishes in,
//!   the run opened cold at every slice finishes too, printing and charged
//!   the same: a cold reload never refuses earlier (R10). One byte under
//!   it, a cold run that refuses has printed a prefix of the literal and
//!   been charged no more than the whole run.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_vm::{
    Bounds, CellStart, CoreMachine, End, Host, Machine, PreparedLibrary, Program, Step, Target,
};

use crate::case::{ExpectedEnd, Shard};
use crate::pages::{BUDGET, OWNER, Source, open};

const COLD_FILES: &[(&str, &str)] = &[
    (
        "r56-cold1.json",
        include_str!("../../probes/cold/r56-cold1.json"),
    ),
    (
        "r56-cold2.json",
        include_str!("../../probes/cold/r56-cold2.json"),
    ),
    (
        "r56-cold3.json",
        include_str!("../../probes/cold/r56-cold3.json"),
    ),
    ("r56-d1.json", include_str!("../../probes/cold/r56-d1.json")),
    ("r56-z1.json", include_str!("../../probes/cold/r56-z1.json")),
];

const PRESSURE: &str = include_str!("../../probes/cold/r56-pressure.json");

/// A host that only prints.
#[derive(Default)]
struct Printer(Vec<Datum>);

impl Host for Printer {
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// What a run left: its end, prints and charge.
#[derive(Debug)]
struct Left {
    end: End,
    printed: Vec<Datum>,
    charged: Option<u64>,
}

/// The harness's default bounds.
fn library() -> PreparedLibrary {
    let release = lash_kernel_releases::helper_release().expect("helper release");
    PreparedLibrary::with_helpers(
        Arc::new(crate::installed_functions().expect("installed functions")),
        Arc::clone(&release.helpers),
    )
}

/// Runs `text` on the core under `bounds`, in slices of `slice`; when
/// `cold`, it seals at every slice return and goes on in a machine opened
/// cold from the root, its program linked again.
fn run(library: &PreparedLibrary, text: &str, bounds: Bounds, slice: u64, cold: bool) -> Left {
    let program = || -> Program { super::probes::linked(library, text).expect("the probe links") };
    let source = Source::default();
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: None,
        owner: OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine =
        CoreMachine::start(program(), bounds, start, Box::new(source.clone())).expect("starts");
    let mut host = Printer::default();
    loop {
        match machine.run(&mut host, slice).expect("the core runs") {
            Step::Ended(end) => {
                return Left {
                    end,
                    printed: host.0,
                    charged: machine.meters().charged,
                };
            }
            Step::Slice if cold => {
                let sealed = machine
                    .seal(BUDGET)
                    .expect("seals")
                    .expect("an unbounded seal completes");
                let root = lash_kernel_vm::kernel_internals::sealed_root(&sealed).clone();
                machine = open(program(), bounds, &root, &source).expect("opens cold");
            }
            Step::Slice => {}
            Step::Parked(_) => panic!("a probe performs nothing"),
        }
    }
}

/// The literal's prints, and whether it finishes with `null`.
fn literal(expected: &crate::case::Expected) -> (&[Datum], bool) {
    (
        &expected.prints,
        expected.end == ExpectedEnd::Finished(Datum::Null),
    )
}

/// COLD-PROBE: the literal is CPython's; the unpaused run is the charge
/// oracle.
#[test]
fn every_r56_cold_probe_holds_opened_cold_at_every_slice() {
    let library = library();
    let mut failures = Vec::new();
    for (file, text) in COLD_FILES {
        let shard: Shard = serde_json::from_str(text).expect(file);
        for case in &shard.cases {
            let (prints, null) = literal(&case.expected);
            let bounds: Bounds = case.environment.bounds.into();
            let whole = run(&library, &case.document, bounds, u64::MAX, false);
            let left = run(
                &library,
                &case.document,
                bounds,
                case.environment.slice,
                true,
            );
            let finished = matches!(&left.end, End::Finished(f) if f.result == Datum::Null);
            if left.printed != prints || finished != null || left.charged != whole.charged {
                failures.push(format!(
                    "{file} {}: printed {:?}, ended {:?}, charged {:?} where unpaused {:?}",
                    case.name, left.printed, left.end, left.charged, whole.charged
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The least memory bound an unpaused core run of `text` finishes under.
fn threshold(library: &PreparedLibrary, text: &str, bounds: Bounds) -> u64 {
    let finishes = |memory| {
        matches!(
            run(library, text, Bounds { memory, ..bounds }, u64::MAX, false).end,
            End::Finished(_)
        )
    };
    let (mut refused, mut passed) = (0u64, bounds.memory);
    let end = run(library, text, bounds, u64::MAX, false).end;
    assert!(matches!(end, End::Finished(_)), "{end:?}");
    while passed - refused > 1 {
        let mid = refused + (passed - refused) / 2;
        if finishes(mid) {
            passed = mid;
        } else {
            refused = mid;
        }
    }
    passed
}

/// PRESSURE: the unpaused run under the same bound is the oracle.
#[test]
fn a_row_opened_cold_at_every_slice_finishes_under_the_bound_an_unpaused_run_needs() {
    let library = library();
    let shard: Shard = serde_json::from_str(PRESSURE).expect("r56-pressure.json");
    let mut failures = Vec::new();
    for case in &shard.cases {
        let (prints, _) = literal(&case.expected);
        let slice = case.environment.slice;
        let bounds: Bounds = case.environment.bounds.into();
        let least = threshold(&library, &case.document, bounds);
        let at = |memory| Bounds { memory, ..bounds };
        let whole = run(&library, &case.document, at(least), u64::MAX, false);
        let cold = run(&library, &case.document, at(least), slice, true);
        if cold.printed != prints || cold.charged != whole.charged {
            failures.push(format!(
                "{} under {least}: cold printed {:?}, ended {:?}, charged {:?} where unpaused {:?}",
                case.name, cold.printed, cold.end, cold.charged, whole.charged
            ));
        }
        let under = run(&library, &case.document, at(least - 1), slice, true);
        let prefix = prints.starts_with(&under.printed);
        let cheaper = under.charged <= whole.charged;
        if !matches!(under.end, End::Finished(_)) && !(prefix && cheaper) {
            failures.push(format!(
                "{} under {}: cold printed {:?}, charged {:?}",
                case.name,
                least - 1,
                under.printed,
                under.charged
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
