//! Durable identity and projection (seams C, D and G of
//! `docs/design/kernel-end-state.md`): an object takes its durable id at
//! the first projection point that saves it, in the canonical walk's order,
//! so what a body allocates on the way never shifts an id; an ordinary
//! park projects what changed and traverses nothing else; and a seal that
//! runs out of budget keeps the run until it completes.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    FunctionRegistry, NativeCall, NativeError, NativeFunction, NewObject, Value, parse_definition,
};

use lash_kernel_state::paged::SealBudget;
use lash_vocabulary::names::FunctionId;

use super::embedder::{Embedder, Setup, int, result};
use crate::conformance::{Checkpoint, checkpoint};
use crate::{Layout, Machine, MachineError, Step};

/// `probe.wrap(x)` answers `[x]`. Its native allocates that list alone;
/// its body, which layout 3 runs, allocates two lists before it.
struct Wrap;
impl NativeFunction for Wrap {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let x = call.args.values()[0];
        Ok(call.new.allocate(NewObject::List(vec![x])))
    }
}

fn wrap(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let definition = parse_definition(
        "function probe.wrap(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n\
         body { let t = [x, x] let u = [t, t] return [x] }\n",
    )
    .unwrap();
    vec![(
        "probe.wrap",
        registry
            .register(definition, Some(std::sync::Arc::new(Wrap)))
            .unwrap(),
    )]
}

/// Objects before, beside and after the probe, one held in a role of the
/// awaited statement and one in a list nested in a map.
const WRAPPED: &str = r#"
main {
  let before = [1]
  let w = probe.wrap(before)
  let after = map{"k": [w, before]}
  let held = [w, perform 0("park") as Any]
  return (list.len(held), list.len(after["k"]))
}
"#;

/// The checkpoint of `WRAPPED` at its park, run under `layout`.
fn parked(layout: Layout) -> Checkpoint {
    let mut embedder = Embedder::with(
        WRAPPED,
        Setup {
            layout,
            probes: Some(wrap),
            ..Setup::default()
        },
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let (root, _) = embedder.checkpoint();
    checkpoint(&root, Box::new(embedder.source.clone())).unwrap()
}

/// PLAN-INDEP over ids (seam C): layout 3 runs the probe's body, which
/// allocates temporaries its native does not, and the parked checkpoint
/// is the one layout 0 parks, ids included, in its continuation bytes and
/// in its graph.
#[test]
fn a_body_s_temporaries_shift_no_durable_id() {
    let native = parked(Layout(0));
    let body = parked(Layout(3));
    assert!(native.ids().len() >= 4, "{:?}", native.ids());
    assert_eq!(native.ids(), body.ids());
    assert_eq!(native, body);
}

/// A retained state of 4,160 lists, then one change to one small list of
/// it and a park.
const RETAINED: &str = r#"
definition "function retained(x: Any) -> Any\nkernel 1\nmetering statements\nbody { let xs = [0] set xs[0] = x return xs }"
main {
  let rows = []
  let i = 0
  while num.lt(i, 64) {
    let group = []
    let j = 0
    while num.lt(j, 64) {
      set group[list.len(group)] = call retained(j)
      set j = num.add(j, 1)
    }
    set rows[list.len(rows)] = group
    set i = num.add(i, 1)
  }
  do perform 0("built") as Any
  let g = rows[7]
  set g[3] = call retained(70)
  do perform 0("changed") as Any
  return list.len(rows)
}
"#;

/// Seam D, invariant 4: an ordinary park performs no global traversal.
/// Between the park after the state is built and the park after one small
/// list of it changes, the projection visits what that list holds and
/// little else, nothing is marked and no census refinement runs, however
/// much is retained. (A changed object is walked whole: one chunk, until
/// swap 2.4(b) gives large objects chunks.)
#[test]
fn an_ordinary_park_projects_what_changed_and_traverses_nothing_else() {
    let mut embedder = Embedder::new(RETAINED);
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder.checkpoint();
    embedder.deliver("built");
    crate::perf_witness::enable_work();
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder.checkpoint();
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert!(
        work.heap.projection_visits <= 160,
        "the park visited {} objects",
        work.heap.projection_visits
    );
    assert_eq!(work.heap.arena_marks, 0, "the park marked the arena");
    assert_eq!(work.heap.refinement_attempts, 0, "the park refined");
    embedder.deliver("changed");
    assert_eq!(result(embedder.run_to_end(&[])), int(64));
}

/// Seam G: a seal that runs out of budget mid-projection gives nothing and
/// keeps every dirty mark; until a seal completes, the run refuses to run,
/// so the ids an abandoned walk assigned cannot depend on when the budget
/// ran out. The completed seal saves what an unbudgeted seal saves.
#[test]
fn a_seal_out_of_budget_holds_the_run_until_it_completes() {
    control_seals_agree();
    let start = |layout| {
        let mut embedder = Embedder::with(
            RETAINED,
            Setup {
                layout,
                ..Setup::default()
            },
        );
        assert!(matches!(embedder.run(), Step::Parked(_)));
        embedder.deliver("built");
        assert!(matches!(embedder.run(), Step::Parked(_)));
        embedder
    };
    let mut whole = start(Layout(0).without_regions());
    let (root, _) = whole.checkpoint();
    let expected = checkpoint(&root, Box::new(whole.source.clone())).unwrap();

    let mut budgeted = start(Layout(0));
    let tight = SealBudget { pages: 1, bytes: 1 };
    assert!(budgeted.machine.seal(tight).unwrap().is_none());
    assert!(matches!(
        budgeted.machine.run(&mut budgeted.world, u64::MAX),
        Err(MachineError::Projecting)
    ));
    let mut seals = 1;
    let sealed = loop {
        seals += 1;
        if let Some(sealed) = budgeted
            .machine
            .seal(SealBudget {
                pages: 4,
                bytes: 16 << 10,
            })
            .unwrap()
        {
            break sealed;
        }
        assert!(seals < 10_000, "the seal makes no progress");
    };
    assert!(seals > 2, "the budget never ran out");
    let root = crate::kernel_internals::sealed_root(&sealed).clone();
    let saved = checkpoint(&root, Box::new(budgeted.source.clone())).unwrap();
    assert_eq!(saved.ids(), expected.ids());
    assert_eq!(saved, expected);
    let certificate = budgeted.machine.accounting();
    budgeted.open_at(&root, certificate, Layout(2), true);
    assert_eq!(budgeted.machine.semantic_checkpoint(), saved);
    budgeted.deliver("changed");
    assert_eq!(result(budgeted.run_to_end(&[])), int(64));
}

fn control_seals_agree() {
    let source = super::layout::control_document(
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, true) return flat.add(value, apply bump())",
    );
    let run = |layout, budgeted| {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                layout,
                probes: Some(super::layout::scalar_probes),
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        let mut saved = Vec::new();
        let end = loop {
            match embedder.run() {
                Step::Ended(end) => break result(end),
                Step::Slice => (),
                Step::Parked(_) => {
                    if budgeted {
                        assert!(
                            embedder
                                .machine
                                .seal(SealBudget { pages: 1, bytes: 1 })
                                .unwrap()
                                .is_none()
                        );
                        assert!(matches!(
                            embedder.machine.run(&mut embedder.world, u64::MAX),
                            Err(MachineError::Projecting)
                        ));
                    }
                    let mut calls = 0;
                    let sealed = loop {
                        calls += 1;
                        assert!(calls < 1000, "a bounded control seal makes progress");
                        let budget = if budgeted {
                            SealBudget {
                                pages: 4,
                                bytes: 16 << 10,
                            }
                        } else {
                            SealBudget {
                                pages: u64::MAX,
                                bytes: u64::MAX,
                            }
                        };
                        if let Some(sealed) = embedder.machine.seal(budget).unwrap() {
                            break sealed;
                        }
                    };
                    let root = crate::kernel_internals::sealed_root(&sealed).clone();
                    let seen = checkpoint(&root, Box::new(embedder.source.clone())).unwrap();
                    let certificate = embedder.machine.accounting();
                    embedder.open_at(&root, certificate, layout, true);
                    assert_eq!(embedder.machine.semantic_checkpoint(), seen);
                    saved.push(seen);
                    embedder.machine.record_charges(tape.clone());
                    embedder.deliver_next();
                }
            }
        };
        (end, saved, lash_kernel_exec::Weight::expand(tape.steps()))
    };
    let reference = run(Layout(0).without_regions(), false);
    assert_eq!(
        reference.1.len(),
        3,
        "two loop parks and the wide call's operand park"
    );
    assert_eq!(run(Layout(0), true), reference);
    assert_eq!(run(Layout(2), true), reference);
}
