//! Observing a run changes none of its semantic results or checkpoint bytes.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_state::paged::SealBudget;
use lash_kernel_value::tokens::encode_body;

use super::embedder::{Embedder, ROOMY, Setup};
use crate::observe::{GuestContext, WorkReport};
use crate::{Bounds, End, Machine, Request, Step};

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

fn setup(slice: u64) -> Setup {
    Setup {
        bounds: PRESSED,
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
    events: Vec<crate::Weight>,
}

#[derive(Debug, Default, PartialEq)]
struct Facts {
    counts: crate::SiteCounts,
    path: crate::ExecutionPath,
    sites: Vec<lash_kernel_value::sites::EffectLocation>,
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
    fn enter_site(&mut self, at: &lash_kernel_value::sites::EffectLocation) {
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
        use_: &lash_kernel_value::OperationUse,
        args: &[Datum],
        site: &lash_kernel_value::sites::PositionLabel,
        origin: &lash_kernel_value::sites::Site,
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
    Full,
}

fn run(source: &str, setup: Setup, observing: Observing) -> (Seen, Option<WorkReport>, Facts) {
    let setup = Setup {
        probes: Some(if source == super::layout::HELPERS {
            super::layout::helper_probes
        } else {
            observation_probes
        }),
        ..setup
    };
    let slice = setup.slice;
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
    seen.events = crate::Weight::expand(tape.steps());
    assert!(
        !seen.events.is_empty(),
        "the observer compares emitted charge facts"
    );

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
/// and sealed roots as they are, whole and in slices.
#[test]
fn observing_a_run_changes_nothing_it_can_observe() {
    for cancel in [false, true] {
        let source = super::scheduler::document(cancel);
        let reference = run(&source, Setup::default(), Observing::Full);
        let actual = run(&source, Setup::default(), Observing::Full);
        assert_eq!(
            actual.0, reference.0,
            "scheduler observation, cancel={cancel}"
        );
        assert_eq!(actual.2, reference.2, "scheduler facts, cancel={cancel}");
    }
    let source = super::layout::control_document(
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, true) return flat.add(value, apply bump())",
    );
    for slice in [u64::MAX, 7] {
        let setup = Setup {
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
            Observing::Full,
        ] {
            let setup = Setup {
                slice,
                probes: Some(super::layout::scalar_probes),
                ..Setup::default()
            };
            assert_eq!(
                run(&source, setup, observing).0,
                plain,
                "control {observing:?}, slice={slice}"
            );
        }
    }
    for source in [PROGRAM, super::layout::ALLOCATING, super::layout::HELPERS] {
        for bounds in if source == PROGRAM {
            vec![PRESSED, ROOMY]
        } else {
            vec![ROOMY]
        } {
            for slice in [u64::MAX, 7] {
                let (plain, none, _) = run(
                    source,
                    Setup {
                        bounds,
                        ..setup(slice)
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
                    Observing::Full,
                ] {
                    let (observed, work, facts) = run(
                        source,
                        Setup {
                            bounds,
                            ..setup(slice)
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
                                ..setup(slice)
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
                            "work facts for {observing:?}, slice {slice}"
                        );
                    }
                    assert_eq!(observed, plain, "{observing:?} in slices of {slice}");
                }
            }
        }
    }
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
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
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

/// FIG-6817: a catch's recovery is retained before it is reported, including
/// a still-open catch and catches in a callee or a callback. Taking the
/// observation twice must not report the same failed occurrence twice.
#[test]
fn caught_failures_survive_a_seal_before_the_host_takes_them() {
    for body in [
        "try { do perform 1(\"failed\") as Any } catch e { do perform 0(\"held\") as Any } return null",
        "let answer = call recover() do perform 0(\"held\") as Any return answer",
        "let answer = invoke each.twice(fn(x) { try { do perform 1(\"failed\") as Any } catch e { return x } }, 7) do perform 0(\"held\") as Any return answer",
    ] {
        let recover = "function recover() -> Any\nkernel 1\nmetering statements\nbody { try { do perform 1(\"failed\") as Any } catch e { return 7 } }";
        let mut run = Embedder::new(&format!("definition {recover:?}\nmain {{ {body} }}"));
        assert!(matches!(run.run(), Step::Parked(_)), "{body}");
        let failed = super::embedder::wait_of(&run.pending[0]);
        run.deliver("failed");
        assert!(matches!(run.run(), Step::Parked(_)), "{body}");
        run.reopen(false);
        assert_eq!(run.machine.take_caught(), [failed], "{body}");
        assert!(run.machine.take_caught().is_empty(), "{body}");
    }
}

/// FIG-6817: reopening an active catch must retain enough error identity
/// to undo its take when it re-raises, also through a callee or callback.
#[test]
fn a_rethrow_after_reopening_an_active_catch_is_not_recovered() {
    let failing =
        "try { do perform 1(\"failed\") as Any } catch e { print \"taken\" throw e } return null";
    let definition =
        format!("function recover() -> Any\nkernel 1\nmetering statements\nbody {{ {failing} }}");
    for body in [
        failing.to_owned(),
        "return call recover()".to_owned(),
        format!("return invoke each.twice(fn(x) {{ {failing} }}, 7)"),
    ] {
        let mut run = Embedder::with(
            &format!("definition {definition:?}\nmain {{ {body} }}"),
            Setup {
                slice: 1,
                ..Setup::default()
            },
        );
        while matches!(run.run(), Step::Slice) {}
        run.deliver("failed");
        while run.world.printed.is_empty() {
            assert!(matches!(run.run(), Step::Slice), "{body}");
        }
        run.reopen(false);
        assert!(
            matches!(
                run.run_to_end(&[]),
                End::Error(crate::RunError::Uncaught { .. })
            ),
            "{body}"
        );
        assert!(run.machine.take_caught().is_empty(), "{body}");
    }
}
