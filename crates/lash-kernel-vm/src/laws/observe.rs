//! Observing a run changes none of its semantic results or checkpoint bytes.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_state::paged::SealBudget;
use lash_vocabulary::tokens::encode_body;

use super::embedder::{Embedder, ROOMY, Setup};
use crate::observe::{GuestContext, WorkReport};
use crate::{Bounds, End, Layout, Machine, Request, Step};

/// Declared functions, closures, tasks, effects in a loop, `try` with
/// `catch` and `finally`, natives on their fast path and their general
/// call, a guarded native, a native that builds an object, kernel bodies
/// that call closures, and memory pressure that collects mid-call.
const PROGRAM: &str = r#"
definition "function helper(x: Any) -> Any\nkernel 1\nmetering statements\nbody {  return num.add(x, 1)  }"

definition "function worker(tag: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let total = 0\n  for step in [1, 2] {\n    try {\n      let got = perform 0(tag) as Any\n      set total = num.add(total, step)\n      let broken = text.concat(total, step)\n    } catch e {\n      print e.kind\n    } finally {\n      print (tag, step)\n    }\n  }\n  return total\n }"

definition "function constant(x: Any) -> Any\nkernel 1\nmetering statements\nbody { return 1 }"

main {
  let literal = call constant(0)
  let callback = invoke each.twice(fn(x) { return 1 }, literal)
  let a = call helper(1)
  let b = call helper(a)
  let spun = work.spin(3)
  let filled = work.fill(2)
  let hs = []
  for tag in ["x", "y"] {
    let h = spawn call worker(tag)
    set hs[list.len(hs)] = h
  }
  let results = join all hs
  let bump = fn(n) { return num.add(n, 2) }
  let twice = invoke each.twice(bump, b)
  let xs = [1, 2, 3]
  let mapped = invoke each.map(bump, xs)
  let doubled = invoke pair.twice(21)
  let i = 0
  while num.lt(i, 40) {
    let pad = work.fill(500)
    set i = num.add(i, 1)
  }
  return (a, b, spun, results, twice, mapped, doubled, i, literal, callback)
}
"#;

const PRESSED: Bounds = Bounds {
    memory: 24 << 10,
    ..ROOMY
};

fn setup(layout: Layout, slice: u64) -> Setup {
    Setup {
        bounds: PRESSED,
        layout,
        slice,
        ..Setup::default()
    }
}

/// What a run shows its embedder: every step, the checkpoint root each
/// park seals, what it printed and requested, and its charge.
#[derive(Debug, PartialEq)]
struct Seen {
    steps: Vec<Step>,
    /// Each park's sealed CheckpointRoot, encoded: its pages by digest.
    exports: Vec<Vec<u8>>,
    printed: Vec<Datum>,
    requests: Vec<Request>,
    charged: u64,
    events: Vec<lash_kernel_exec::Weight>,
}

/// How a run is observed.
#[derive(Clone, Copy, Debug)]
enum Observing {
    Nothing,
    Work,
    Context,
    Both,
    Census,
}

fn run(source: &str, setup: Setup, observing: Observing) -> (Seen, Option<WorkReport>) {
    let setup = if source == super::layout::ALLOCATING {
        Setup {
            probes: Some(super::layout::allocation_probes),
            ..setup
        }
    } else {
        setup
    };
    let mut embedder = Embedder::with(source, setup);
    let tape = crate::ChargeTape::new();
    embedder.machine.record_charges(tape.clone());
    let context = GuestContext::new();
    if matches!(observing, Observing::Work | Observing::Both) {
        embedder.machine.count_work();
    }
    if matches!(observing, Observing::Context | Observing::Both) {
        embedder.machine.publish_context(Arc::clone(&context));
    }
    if matches!(observing, Observing::Census) {
        crate::perf_witness::execution::enable();
    }
    let mut seen = Seen {
        steps: Vec::new(),
        exports: Vec::new(),
        printed: Vec::new(),
        requests: Vec::new(),
        charged: 0,
        events: Vec::new(),
    };
    loop {
        let step = embedder.run();
        seen.steps.push(step.clone());
        match step {
            Step::Ended(_) => break,
            Step::Slice => {}
            Step::Parked(_) => {
                let sealed = embedder
                    .machine
                    .seal(SealBudget {
                        pages: u64::MAX,
                        bytes: u64::MAX,
                    })
                    .unwrap()
                    .unwrap();
                seen.exports.push(encode_body(&sealed.root));
                let next = super::embedder::label(&embedder.pending[0]);
                embedder.deliver(&next);
            }
        }
    }
    seen.printed = std::mem::take(&mut embedder.world.printed);
    seen.requests = std::mem::take(&mut embedder.requests);
    seen.charged = embedder.machine.meters().charged.expect("exact charge");
    seen.events = lash_kernel_exec::Weight::expand(tape.steps());
    assert!(
        !seen.events.is_empty(),
        "the observer compares emitted charge facts"
    );
    if matches!(observing, Observing::Census) {
        let census = crate::perf_witness::execution::finish();
        assert!(census.statements.values().sum::<u64>() > 0);
        assert!(!census.transitions.is_empty());
        // Fully admitted runs may have no slow exit. The slice boundary's
        // exact rejecting reason is pinned by the census law below.
    }
    (seen, embedder.machine.work())
}

/// Observing a run, by counting its work or publishing its context or
/// both, leaves its results, failures, charges, parks, prints, requests
/// and sealed roots as they are: under the natural layout and one that
/// takes every general native call, whole and in slices.
#[test]
fn observing_a_run_changes_nothing_it_can_observe() {
    let source = super::layout::control_document(
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, true) return flat.add(value, apply bump())",
    );
    for layout in [Layout(0), Layout(2)] {
        for slice in [u64::MAX, 7] {
            let setup = Setup {
                layout,
                slice,
                probes: Some(super::layout::scalar_probes),
                ..Setup::default()
            };
            let (plain, _) = run(&source, setup, Observing::Nothing);
            assert!(matches!(
                plain.steps.last(),
                Some(Step::Ended(End::Finished(_)))
            ));
            for observing in [
                Observing::Work,
                Observing::Context,
                Observing::Both,
                Observing::Census,
            ] {
                let setup = Setup {
                    layout,
                    slice,
                    probes: Some(super::layout::scalar_probes),
                    ..Setup::default()
                };
                assert_eq!(
                    run(&source, setup, observing).0,
                    plain,
                    "control {observing:?}, {layout:?}, slice={slice}"
                );
            }
        }
    }
    for source in [PROGRAM, super::layout::ALLOCATING] {
        for layout in [Layout::default(), Layout(1)] {
            for slice in [u64::MAX, 7] {
                let (plain, none) = run(
                    source,
                    Setup {
                        bounds: if source == PROGRAM { PRESSED } else { ROOMY },
                        ..setup(layout, slice)
                    },
                    Observing::Nothing,
                );
                assert!(none.is_none());
                assert!(matches!(
                    plain.steps.last(),
                    Some(Step::Ended(End::Finished(_)))
                ));
                assert!(
                    plain.exports.len() >= if source == PROGRAM { 4 } else { 2 },
                    "the run parks at each echo"
                );
                for observing in [
                    Observing::Work,
                    Observing::Context,
                    Observing::Both,
                    Observing::Census,
                ] {
                    let (observed, _) = run(
                        source,
                        Setup {
                            bounds: if source == PROGRAM { PRESSED } else { ROOMY },
                            ..setup(layout, slice)
                        },
                        observing,
                    );
                    assert_eq!(
                        observed, plain,
                        "{observing:?} under layout {layout:?} in slices of {slice}"
                    );
                }
            }
        }
    }
}

/// The census counts executed statements, including a frameless literal return,
/// once under each physical layout. Charge, slicing and the physical owner are
/// independent coordinates; a source continuation is not another statement.
#[test]
fn the_execution_census_counts_statements_across_owner_handoffs() {
    use crate::perf_witness::execution::{self, Owner};
    // Reusing the typed kernel does not hand register statements to a region.
    let source = super::layout::scalar_document(
        "let i = 0.0 while flat.lt(i, 4.0) { let previous = flat.identity(i) set i = flat.add(previous, 1.0) } return i",
    );
    for record in [false, true] {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                probes: Some(super::layout::scalar_probes),
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        if record {
            embedder.machine.record_charges(tape);
        }
        crate::machine::islands::TYPED_REPEATED.with(|n| n.set(0));
        execution::enable();
        assert!(matches!(embedder.run_to_end(&[]), End::Finished(_)));
        let census = execution::finish();
        assert!(crate::machine::islands::TYPED_REPEATED.with(|n| n.get()) > 0);
        assert_eq!(census.statements.values().sum::<u64>(), 13, "{census:?}");
        assert!(
            census
                .statements
                .get(&Owner::RegisterProgram)
                .copied()
                .unwrap_or(0)
                >= 11,
            "{census:?}"
        );
        assert_eq!(
            census
                .statements
                .get(&Owner::ScalarRegion)
                .copied()
                .unwrap_or(0),
            0,
            "{census:?}"
        );
    }
    const TEXT: &str = r#"
definition "function literal() -> Any\nkernel 1\nmetering statements\nbody { return 7 }"
main {
    let answer = call literal()
    let i = 0
    while num.lt(i, 3) {
        set i = num.add(i, 1)
    }
    return answer
}
"#;
    // Three main statements before the loop body, three body writes, the
    // final return and the literal callee's return: eight guest statements total. Helper-body statements are counted separately.
    for allocating in [false, true] {
        let text = if allocating {
            TEXT.replace("        set i", "        let padding = [i]\n        set i")
        } else {
            TEXT.to_owned()
        };
        let expected = if allocating { 11 } else { 8 };
        for layout in [
            Layout::default(),
            Layout::default().without_regions(),
            Layout(1),
            Layout(1).without_regions(),
        ] {
            for slice in [u64::MAX, 1] {
                let mut embedder = Embedder::with(
                    &text,
                    Setup {
                        layout,
                        slice,
                        ..Setup::default()
                    },
                );
                let tape = crate::ChargeTape::new();
                embedder.machine.record_charges(tape.clone());
                execution::enable();
                loop {
                    match embedder.run() {
                        Step::Slice => {}
                        Step::Ended(End::Finished(_)) => break,
                        other => panic!("unexpected execution: {other:?}"),
                    }
                }
                let census = execution::finish();
                assert_eq!(census.register_entries, tape.work().calls);
                if slice == 1 {
                    let reason = if layout.regions() {
                        execution::Reason::Slice
                    } else {
                        execution::Reason::NotAdmitted
                    };
                    assert!(
                        census
                            .slow_exits
                            .values()
                            .any(|reasons| reasons.get(&reason).is_some_and(|count| *count > 0)),
                        "register entry reports {reason:?} at its rejecting boundary: {census:?}"
                    );
                }
                assert_eq!(
                    census
                        .statements
                        .iter()
                        .filter(|(owner, _)| !matches!(
                            owner,
                            Owner::HelperPlan | Owner::SourceHelperFrame
                        ))
                        .map(|(_, count)| count)
                        .sum::<u64>(),
                    expected,
                    "{layout:?}, slice {slice}: {census:?}"
                );
                if layout == Layout::default() && slice == u64::MAX {
                    if allocating {
                        assert!(
                            census
                                .statements
                                .get(&Owner::AllocatingRegion)
                                .copied()
                                .unwrap_or(0)
                                > 0,
                            "{census:?}"
                        );
                    }
                    assert!(census.register_entries > 0, "{census:?}");
                    assert!(
                        census
                            .statements
                            .get(&Owner::RegisterProgram)
                            .copied()
                            .unwrap_or(0)
                            > 0
                    );
                    assert!(
                        census
                            .transitions
                            .values()
                            .flat_map(|row| row.values())
                            .sum::<u64>()
                            >= 2
                    );
                }
                // Finishing disables the producer; the next capture starts empty.
                assert_eq!(execution::finish(), execution::Census::default());
            }
        }
    }
    // Each source-helper statement hands control back to the general driver,
    // even when the next ready statement belongs to that same helper frame.
    let mut embedder = Embedder::with(
        "main { let got = invoke each.twice(fn(x) { return x }, 1) return got }",
        Setup::default(),
    );
    execution::enable();
    assert!(matches!(embedder.run(), Step::Ended(End::Finished(_))));
    let census = execution::finish();
    let helpers = census.statements[&Owner::SourceHelperFrame];
    assert!(helpers > 1);
    let handoffs = census
        .transitions
        .get(&Owner::GeneralStatementDriver)
        .and_then(|row| row.get(&Owner::SourceHelperFrame))
        .copied()
        .unwrap_or(0);
    assert!(
        handoffs >= helpers,
        "every source-helper statement returns to its driver: {census:?}"
    );
}
