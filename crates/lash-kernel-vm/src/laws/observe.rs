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
  let select = fn(x) { if num.lt(x, 0) {} else { return x } return 0 }
  let chosen = apply select(1)
  let empty = apply select(-1)
  let tag = fn(x) { return same(kind(x), "int") }
  let unit = fn(x) { return same(x, ()) }
  let facts = (apply tag(1), apply tag(2), apply unit(1), apply unit(()))
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

#[derive(Debug, Default, PartialEq)]
struct Facts {
    counts: crate::SiteCounts,
    path: crate::ExecutionPath,
    sites: Vec<lash_vocabulary::sites::EffectLocation>,
    samples: Vec<(
        u64,
        u32,
        crate::observe::GuestPhase,
        Vec<crate::observe::GuestFrame>,
    )>,
}

struct AtSites<'a> {
    world: &'a mut super::embedder::World,
    facts: &'a mut Facts,
    context: &'a GuestContext,
}

impl crate::Host for AtSites<'_> {
    fn cancel_requested(&mut self) -> bool {
        crate::Host::cancel_requested(self.world)
    }
    fn observes_sites(&self) -> bool {
        true
    }
    fn enter_site(&mut self, at: &lash_vocabulary::sites::EffectLocation) {
        self.facts.sites.push(at.clone());
        let mut sample = crate::observe::GuestSample::new();
        self.context.read(&mut sample).unwrap();
        self.facts
            .samples
            .push((sample.task, sample.depth, sample.phase, sample.frames));
    }
    fn print(&mut self, value: &Datum) {
        crate::Host::print(self.world, value);
    }
    fn perform(
        &mut self,
        use_: &lash_vocabulary::OperationUse,
        args: &[Datum],
        site: &lash_vocabulary::sites::PositionLabel,
        origin: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, crate::HostFault> {
        crate::Host::perform(self.world, use_, args, site, origin)
    }
}

/// How a run is observed.
#[derive(Clone, Copy, Debug)]
enum Observing {
    Nothing,
    Work,
    Context,
    Both,
    Census,
    Full,
}

fn run(source: &str, setup: Setup, observing: Observing) -> (Seen, Option<WorkReport>, Facts) {
    let setup = Setup {
        probes: Some(observation_probes),
        ..setup
    };
    let (layout, slice, bounds) = (setup.layout, setup.slice, setup.bounds);
    let mut embedder = Embedder::with(source, setup);
    let tape = crate::ChargeTape::new();
    embedder.machine.record_charges(tape.clone());
    let context = GuestContext::new();
    if matches!(
        observing,
        Observing::Work | Observing::Both | Observing::Full
    ) {
        embedder.machine.count_work();
    }
    if matches!(
        observing,
        Observing::Context | Observing::Both | Observing::Full
    ) {
        embedder.machine.publish_context(Arc::clone(&context));
    }
    let mut facts = Facts::default();
    if matches!(observing, Observing::Full) {
        embedder.machine.trace_path();
        embedder.machine.set_observe(crate::ObservationMode::Counts);
    }
    embedder.machine.count_execution();
    let mut seen = Seen {
        steps: Vec::new(),
        exports: Vec::new(),
        printed: Vec::new(),
        requests: Vec::new(),
        charged: 0,
        events: Vec::new(),
    };
    loop {
        let step = if matches!(observing, Observing::Full) {
            let step = embedder
                .machine
                .run(
                    &mut AtSites {
                        world: &mut embedder.world,
                        facts: &mut facts,
                        context: &context,
                    },
                    slice,
                )
                .unwrap();
            if let Step::Parked(park) = &step {
                embedder.requests.extend(park.requests.iter().cloned());
                embedder.pending.extend(park.requests.iter().cloned());
            }
            step
        } else {
            embedder.run()
        };
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
    {
        let census = embedder.machine.take_execution_census();
        if bounds.memory == ROOMY.memory && layout == Layout::default() && slice == u64::MAX {
            assert!(
                census.register_entries > 0
                    && census
                        .statements
                        .get(&crate::perf_witness::execution::Owner::RegisterProgram)
                        .copied()
                        .unwrap_or(0)
                        > 0,
                "{observing:?} executes the register program: {census:?}"
            );
        }
        assert!(
            census
                .slow_exits
                .get(&crate::perf_witness::execution::Owner::RegisterProgram)
                .is_none_or(|reasons| !reasons
                    .contains_key(&crate::perf_witness::execution::Reason::Observation)),
            "passive observation never rejects register admission: {census:?}"
        );
        assert!(census.statements.values().sum::<u64>() > 0);
        assert!(!census.transitions.is_empty());
        // Fully admitted runs may have no slow exit. The slice boundary's
        // exact rejecting reason is pinned by the census law below.
    }
    if matches!(observing, Observing::Full) {
        facts.counts = embedder.machine.take_ran();
        facts.path = embedder.machine.take_execution_path().unwrap();
        assert!(!facts.counts.is_empty());
        assert!(!facts.sites.is_empty());
    }
    (seen, embedder.machine.work(), facts)
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
            let (plain, _, _) = run(&source, setup, Observing::Nothing);
            assert!(matches!(
                plain.steps.last(),
                Some(Step::Ended(End::Finished(_)))
            ));
            for observing in [
                Observing::Work,
                Observing::Context,
                Observing::Both,
                Observing::Census,
                Observing::Full,
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
        for bounds in if source == PROGRAM {
            vec![PRESSED, ROOMY]
        } else {
            vec![ROOMY]
        } {
            for layout in [Layout::default(), Layout(1)] {
                for slice in [u64::MAX, 7] {
                    let (plain, none, _) = run(
                        source,
                        Setup {
                            bounds,
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
                        Observing::Full,
                    ] {
                        let (observed, work, facts) = run(
                            source,
                            Setup {
                                bounds,
                                ..setup(layout, slice)
                            },
                            observing,
                        );
                        if matches!(
                            observing,
                            Observing::Work | Observing::Both | Observing::Full
                        ) {
                            let (reference, reference_work, reference_facts) = run(
                                source,
                                Setup {
                                    bounds,
                                    ..setup(layout.without_regions(), slice)
                                },
                                observing,
                            );
                            assert_eq!(observed, reference);
                            assert_eq!(
                                facts, reference_facts,
                                "passive site/count/path/context facts"
                            );
                            assert_eq!(
                                work, reference_work,
                                "work facts for {observing:?}, {layout:?}, slice {slice}"
                            );
                        }
                        assert_eq!(
                            observed, plain,
                            "{observing:?} under layout {layout:?} in slices of {slice}"
                        );
                    }
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
        embedder.machine.count_execution();
        assert!(matches!(embedder.run_to_end(&[]), End::Finished(_)));
        let census = embedder.machine.take_execution_census();
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
                embedder.machine.count_execution();
                loop {
                    match embedder.run() {
                        Step::Slice => {}
                        Step::Ended(End::Finished(_)) => break,
                        other => panic!("unexpected execution: {other:?}"),
                    }
                }
                let census = embedder.machine.take_execution_census();
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
                assert_eq!(
                    embedder.machine.take_execution_census(),
                    execution::Census::default()
                );
            }
        }
    }
    // ADR 0123: another hosted machine neither contributes to this census
    // nor resets it, including when its own collection starts and finishes.
    let mut first = Embedder::with("main { return 7 }", Setup::default());
    let mut second = Embedder::with("main { let x = 1 return x }", Setup::default());
    first.machine.count_execution();
    second.machine.count_execution();
    assert!(matches!(second.run_to_end(&[]), End::Finished(_)));
    let other = second.machine.take_execution_census();
    assert_eq!(other.statements.values().sum::<u64>(), 2);
    assert!(matches!(first.run_to_end(&[]), End::Finished(_)));
    let own = first.machine.take_execution_census();
    assert_eq!(own.statements.values().sum::<u64>(), 1);
    assert_eq!(
        second.machine.take_execution_census(),
        execution::Census::default()
    );
    // Each source-helper statement hands control back to the general driver,
    // even when the next ready statement belongs to that same helper frame.
    let mut embedder = Embedder::with(
        "main { let got = invoke each.twice(fn(x) { return x }, 1) return got }",
        Setup::default(),
    );
    embedder.machine.count_execution();
    assert!(matches!(embedder.run(), Step::Ended(End::Finished(_))));
    let census = embedder.machine.take_execution_census();
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

/// Opt-in observation stays derived across projection, refusal and cold reopen.
pub(super) fn fully_observe(machine: &mut crate::KernelMachine) {
    machine.count_work();
    machine.publish_context(GuestContext::new());
    machine.trace_path();
    machine.set_observe(crate::ObservationMode::Counts);
}

fn observation_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    let mut probes = super::layout::allocation_probes(registry, ids);
    probes.extend(super::layout::scalar_probes(registry, ids));
    probes.extend(
        lash_kernel_lib::numbers()
            .into_iter()
            .filter_map(|(definition, native)| {
                let name = match definition.name.as_str() {
                    "kind" => "kind",
                    "same" => "same",
                    _ => return None,
                };
                Some((name, registry.register(definition, Some(native)).unwrap()))
            }),
    );
    probes
}
