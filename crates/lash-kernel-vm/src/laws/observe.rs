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
}

fn run(setup: Setup, observing: Observing) -> (Seen, Option<WorkReport>) {
    let mut embedder = Embedder::with(PROGRAM, setup);
    let tape = crate::ChargeTape::new();
    embedder.machine.record_charges(tape.clone());
    let context = GuestContext::new();
    if matches!(observing, Observing::Work | Observing::Both) {
        embedder.machine.count_work();
    }
    if matches!(observing, Observing::Context | Observing::Both) {
        embedder.machine.publish_context(Arc::clone(&context));
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
    (seen, embedder.machine.work())
}

/// Observing a run, by counting its work or publishing its context or
/// both, leaves its results, failures, charges, parks, prints, requests
/// and sealed roots as they are: under the natural layout and one that
/// takes every general native call, whole and in slices.
#[test]
fn observing_a_run_changes_nothing_it_can_observe() {
    for layout in [Layout::default(), Layout(1)] {
        for slice in [u64::MAX, 7] {
            let (plain, none) = run(setup(layout, slice), Observing::Nothing);
            assert!(none.is_none());
            assert!(matches!(
                plain.steps.last(),
                Some(Step::Ended(End::Finished(_)))
            ));
            assert!(plain.exports.len() >= 4, "the run parks at each echo");
            for observing in [Observing::Work, Observing::Context, Observing::Both] {
                let (observed, _) = run(setup(layout, slice), observing);
                assert_eq!(
                    observed, plain,
                    "{observing:?} under layout {layout:?} in slices of {slice}"
                );
            }
        }
    }
}
