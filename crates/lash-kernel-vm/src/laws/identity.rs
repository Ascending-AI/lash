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
use lash_kernel_value::names::FunctionId;

use super::embedder::{Embedder, Setup, int, result};
use crate::{Machine, Step};
use lash_kernel_state::semantic::{Checkpoint, checkpoint};

/// `probe.wrap(x)` answers `[x]`. Its native allocates that list alone;
/// its body allocates two lists before it.
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
    wrap_registered(registry, true)
}

/// `probe.wrap` registered without its native: its body runs.
fn wrap_body(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    wrap_registered(registry, false)
}

fn wrap_registered(
    registry: &mut FunctionRegistry,
    native: bool,
) -> Vec<(&'static str, FunctionId)> {
    let definition = parse_definition(
        "function probe.wrap(x: Any) -> Any\nkernel 1\ncharge 1\nnative\n\
         body { let t = [x, x] let u = [t, t] return [x] }\n",
    )
    .unwrap();
    vec![(
        "probe.wrap",
        registry
            .register(
                definition,
                native.then(|| std::sync::Arc::new(Wrap) as std::sync::Arc<dyn NativeFunction>),
            )
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

/// The checkpoint of `WRAPPED` at its park, with `probe.wrap`'s native
/// registered or its body run.
fn parked(native: bool) -> Checkpoint {
    let mut embedder = Embedder::with(
        WRAPPED,
        Setup {
            probes: Some(if native { wrap } else { wrap_body }),
            ..Setup::default()
        },
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let root = embedder.checkpoint();
    checkpoint(&root, Box::new(embedder.source.clone())).unwrap()
}

/// PLAN-INDEP over ids (seam C): the probe's body allocates temporaries
/// its native does not, and the parked checkpoint is the one the native
/// parks, ids included, in its continuation bytes and
/// in its graph.
#[test]
fn a_body_s_temporaries_shift_no_durable_id() {
    let native = parked(true);
    let body = parked(false);
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

/// Seam G: a seal that runs out of budget publishes no root, and the ids
/// a completed seal assigns do not depend on when the budget ran out: the
/// seal that completes after budgeted calls saves what an unbudgeted seal
/// saves, ids included.
#[test]
fn a_seal_out_of_budget_publishes_no_root_and_assigns_unbudgeted_ids() {
    scheduler_seals_agree();
    control_seals_agree();
    helper_seals_agree();
    let start = || {
        let mut embedder = Embedder::with(RETAINED, Setup::default());
        super::observe::fully_observe(&mut embedder.machine);

        assert!(matches!(embedder.run(), Step::Parked(_)));
        embedder.deliver("built");
        assert!(matches!(embedder.run(), Step::Parked(_)));

        embedder
    };
    let mut whole = start();
    let root = whole.checkpoint();
    let expected = checkpoint(&root, Box::new(whole.source.clone())).unwrap();
    let expected_charge = whole.machine.meters().charged;
    let expected_work = whole.machine.work();

    let mut budgeted = start();
    assert_eq!(budgeted.machine.meters().charged, expected_charge);
    assert_eq!(budgeted.machine.work(), expected_work);
    let tight = SealBudget { pages: 1, bytes: 1 };
    assert!(budgeted.machine.seal(tight).unwrap().is_none());
    let mut seals = 1;
    let sealed = loop {
        seals += 1;
        if let Some(sealed) = budgeted.machine.seal(tight).unwrap() {
            break sealed;
        }
        assert!(seals < 10_000, "the seal makes no progress");
    };
    assert!(seals > 2, "the budget never ran out");
    let root = crate::kernel_internals::sealed_root(&sealed).clone();
    let saved = checkpoint(&root, Box::new(budgeted.source.clone())).unwrap();
    assert_eq!(saved.ids(), expected.ids());
    assert_eq!(saved, expected);
    budgeted.open_at(&root, true);
    assert_eq!(budgeted.machine.semantic_checkpoint(), saved);
    assert!(
        budgeted.machine.work().is_none(),
        "observations are never stored"
    );
    super::observe::fully_observe(&mut budgeted.machine);
    budgeted.deliver("changed");
    assert_eq!(result(budgeted.run_to_end(&[])), int(64));
}

fn control_seals_agree() {
    let source = super::layout::control_document(
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, true) return flat.add(value, apply bump())",
    );
    let run = |budgeted| {
        let mut embedder = Embedder::with(
            &source,
            Setup {
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
                    embedder.open_at(&root, true);
                    assert_eq!(embedder.machine.semantic_checkpoint(), seen);
                    saved.push(seen);
                    embedder.machine.record_charges(tape.clone());
                    embedder.deliver_next();
                }
            }
        };
        (end, saved, crate::Weight::expand(tape.steps()))
    };
    let reference = run(false);
    assert_eq!(
        reference.1.len(),
        3,
        "two loop parks and the wide call's operand park"
    );
    assert_eq!(run(true), reference);
}

// K-EVAL-008 / K-TASK-017: a waiting or cancelling task owns the same
// operand and cleanup identities while a bounded seal holds the machine.
fn scheduler_seals_agree() {
    for cancel in [false, true] {
        let source = super::scheduler::document(cancel);
        let run = |bounded: bool| {
            let mut embedder = Embedder::with(&source, Setup::default());

            let mut saved = Vec::new();

            let end = loop {
                match embedder.run() {
                    Step::Ended(mut end) => {
                        if let crate::End::Finished(done) = &mut end {
                            done.carry.take();
                        }
                        break end;
                    }
                    Step::Slice => (),
                    Step::Parked(_) => {
                        if bounded {
                            assert!(
                                embedder
                                    .machine
                                    .seal(SealBudget { pages: 1, bytes: 1 })
                                    .unwrap()
                                    .is_none()
                            );
                        }
                        let mut attempts = 0;
                        let sealed = loop {
                            attempts += 1;
                            assert!(attempts < 1000);
                            if let Some(sealed) = embedder
                                .machine
                                .seal(SealBudget {
                                    pages: if bounded { 4 } else { u64::MAX },
                                    bytes: if bounded { 16 << 10 } else { u64::MAX },
                                })
                                .unwrap()
                            {
                                break sealed;
                            }
                        };
                        let root = crate::kernel_internals::sealed_root(&sealed).clone();
                        let seen = checkpoint(&root, Box::new(embedder.source.clone())).unwrap();
                        embedder.open_at(&root, true);
                        assert_eq!(embedder.machine.semantic_checkpoint(), seen);
                        saved.push(seen);

                        embedder.deliver_next();
                    }
                }
            };

            (
                end,
                saved,
                embedder.requests,
                embedder.machine.meters().charged,
            )
        };
        let reference = run(false);
        assert!(!reference.1.is_empty());
        assert_eq!(run(true), reference);
    }
}

fn helper_seals_agree() {
    let run = |budgeted| {
        let mut embedder = Embedder::with(
            super::layout::HELPERS,
            Setup {
                probes: Some(super::layout::helper_probes),
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
                    }
                    let root = loop {
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
                            break crate::kernel_internals::sealed_root(&sealed).clone();
                        }
                    };
                    let seen = checkpoint(&root, Box::new(embedder.source.clone())).unwrap();
                    embedder.open_at(&root, true);
                    assert_eq!(embedder.machine.semantic_checkpoint(), seen);
                    saved.push(seen);
                    embedder.machine.record_charges(tape.clone());

                    embedder.deliver_next();
                }
            }
        };

        (end, saved, crate::Weight::expand(tape.steps()))
    };
    let reference = run(false);
    assert_eq!(
        reference.1.len(),
        5,
        "library continuation, catch, both finally departures, captured cells"
    );
    let actual = run(true);
    for (actual, expected) in actual.1.iter().zip(&reference.1) {
        assert_eq!(actual.ids(), expected.ids());
    }
    assert_eq!(actual, reference);
}

/// Every effect identity a run asks for, answering each from the laws'
/// table in the order asked: on the frozen engine and on the bytecode core.
fn effect_identities(text: &str) -> [Vec<lash_kernel_value::sites::EffectIdentity>; 2] {
    use super::embedder::{World, answer, core_machine, wait_of};
    use lash_kernel_value::kernel_internals::effect_parts;
    let parts = |requests: &[crate::Request]| {
        requests
            .iter()
            .map(|request| {
                let crate::Request::Effect(effect) = request;
                effect_parts(&effect.identity).clone()
            })
            .collect::<Vec<_>>()
    };
    let mut base = Embedder::with(text, Setup::default());
    let end = base.run_to_end(&[]);
    assert_eq!(result(end), int(0), "{text}");
    let mut core = core_machine(text, super::embedder::ROOMY, None).unwrap();
    let mut world = World::default();
    let mut asked = Vec::new();
    loop {
        match core.run(&mut world, u64::MAX).unwrap() {
            Step::Ended(end) => {
                assert_eq!(result(end), int(0), "{text}");
                break;
            }
            Step::Slice => {}
            Step::Parked(park) => {
                for request in &park.requests {
                    asked.push(request.clone());
                    core.deliver(wait_of(request), answer(request)).unwrap();
                }
            }
        }
    }
    [parts(&base.requests), parts(&asked)]
}

/// `K-EFF-008`, `K-TASK-020`, `K-SITE-004`, literal: an effect is named by
/// its task, its `perform`'s site, how often that task ran the site before
/// (counted across every call, here `g`'s through `f`'s) and the loops
/// around it outermost first across the task's calls: a `while` and the
/// `for` in its body, then a `for … via` whose walk `work.fill` opens. A
/// spawned task is named by its spawner, the spawn's site and occurrence;
/// it counts its own effects from 0 and runs inside no loop of its
/// spawner's, and a task it spawns names it as parent. Both engines ask
/// for exactly these.
#[test]
fn effects_are_named_by_task_site_occurrence_and_enclosing_loops() {
    use super::embedder::{function_site, main_site};
    use lash_kernel_value::sites::{EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};
    const G: &str = "definition \"function g(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let v = perform 0(x) as Any return v }\"\n";
    let looped = format!(
        "{G}definition \"function f(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{ let a = call g(x) let b = call g(x) return b }}\"\n\
         main {{\n  let i = 0\n  while num.lt(i, 2) {{\n    for x in [7, 8] {{\n      let r = call f(x)\n    }}\n    set i = num.add(i, 1)\n  }}\n  for y in 2 via (open work.fill) {{\n    let s = call g(y)\n  }}\n  return 0\n}}"
    );
    // A law's function ids are known once its document is parsed.
    let asked = effect_identities(&looped);
    let at = |path: Vec<u32>, iteration| LoopIteration {
        site: lash_kernel_value::sites::Site::new(lash_kernel_value::sites::Unit::Main, path),
        iteration,
    };
    let effect = |task: TaskIdentity, occurrence, loops| EffectIdentity {
        task,
        site: function_site("g", [0, 0]),
        occurrence,
        loops,
    };
    let mut expected = Vec::new();
    for i in 0..2 {
        for x in 0..2 {
            for _call in 0..2 {
                let occurrence = expected.len() as u64;
                expected.push(effect(
                    TaskIdentity::Main,
                    occurrence,
                    vec![at(vec![1], i), at(vec![1, 1, 0], x)],
                ));
            }
        }
    }
    for y in 0..2 {
        expected.push(effect(TaskIdentity::Main, 8 + y, vec![at(vec![2], y)]));
    }
    assert_eq!(asked, [expected.clone(), expected]);

    let spawned = format!(
        "{G}definition \"function k(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {{ let h = spawn call g(x) let v = join h return v }}\"\n\
         main {{\n  let hs = []\n  for x in [1, 2] {{\n    let h = spawn call g(x)\n    set hs[list.len(hs)] = h\n  }}\n  let c = spawn call k(3)\n  set hs[list.len(hs)] = c\n  do join all hs\n  return 0\n}}"
    );
    let asked = effect_identities(&spawned);
    let child = |site, occurrence| {
        TaskIdentity::Spawned(SpawnIdentity {
            parent: std::sync::Arc::new(TaskIdentity::Main),
            site,
            occurrence,
        })
    };
    let grandchild = TaskIdentity::Spawned(SpawnIdentity {
        parent: std::sync::Arc::new(child(main_site([2, 0]), 0)),
        site: function_site("k", [0, 0]),
        occurrence: 0,
    });
    let expected = vec![
        effect(child(main_site([1, 1, 0, 0]), 0), 0, Vec::new()),
        effect(child(main_site([1, 1, 0, 0]), 1), 0, Vec::new()),
        effect(grandchild, 0, Vec::new()),
    ];
    assert_eq!(asked, [expected.clone(), expected]);
}

/// `K-EFF-008`: a task counts each call action of document code it
/// enters, as it counts its performs. A host that observes sites sees each
/// entry named by its task, its site, how often the task entered that site
/// before and the loops around it. The counts are sealed with the task, so
/// a machine opened cold from the root goes on counting where the seal
/// left off. Observing seals the same bytes as not observing. A call its
/// arity refuses was entered all the same, as the frozen engine counted it.
#[test]
fn entered_calls_are_counted_across_a_seal_and_a_cold_reopen() {
    use lash_kernel_doc::{Datum, ErrorDatum};
    use lash_kernel_value::kernel_internals::effect_parts;
    use lash_kernel_value::sites::{
        EffectIdentity, EffectLocation, LoopIteration, PositionLabel, Site, TaskIdentity,
    };

    use super::embedder::{ROOMY, Source, World, answer, core_program, main_site, wait_of};
    use crate::{CoreMachine, HostFault, Request};

    /// A host that observes sites, keeping each entry.
    #[derive(Default)]
    struct Observing {
        world: World,
        entered: Vec<EffectLocation>,
    }
    impl crate::Host for Observing {
        fn perform(
            &mut self,
            effect: &lash_kernel_value::OperationUse,
            args: &[Datum],
            site: &PositionLabel,
            origin: &Site,
        ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
            crate::Host::perform(&mut self.world, effect, args, site, origin)
        }
        fn print(&mut self, value: &Datum) {
            crate::Host::print(&mut self.world, value);
        }
        fn cancel_requested(&mut self) -> bool {
            crate::Host::cancel_requested(&mut self.world)
        }
        fn observes_sites(&self) -> bool {
            true
        }
        fn enter_site(&mut self, at: &EffectLocation) {
            self.entered.push(at.clone());
        }
    }

    let text = "main {\n  let id = fn(x) { return x }\n  for x in [1, 2] {\n    let r = apply id(x)\n    try { let q = apply id(x, x) } catch e { }\n    let v = perform 0(r) as Any\n  }\n  return 0\n}";
    let source = Source::default();
    let start = || {
        <CoreMachine as Machine>::start(
            core_program(text, None),
            ROOMY,
            Setup::default().start,
            Box::new(source.clone()),
        )
        .unwrap()
    };
    let parked = |step: Step| -> Request {
        let Step::Parked(park) = step else {
            panic!("the run parks at its perform: {step:?}");
        };
        let [request] = &park.requests[..] else {
            panic!("one request at the park");
        };
        request.clone()
    };
    let seal = |machine: &mut CoreMachine| {
        let budget = SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        };
        crate::kernel_internals::sealed_root(&machine.seal(budget).unwrap().unwrap()).clone()
    };

    let mut host = Observing::default();
    let mut observed = start();
    let request = parked(observed.run(&mut host, u64::MAX).unwrap());
    let mut plain = start();
    parked(plain.run(&mut World::default(), u64::MAX).unwrap());
    let root = seal(&mut observed);
    assert_eq!(
        lash_kernel_value::tokens::encode_body(&root),
        lash_kernel_value::tokens::encode_body(&seal(&mut plain)),
        "observing seals the same bytes"
    );

    let mut reopened = <CoreMachine as Machine>::open(
        core_program(text, None),
        ROOMY,
        &lash_kernel_state::kernel_internals::checkpoint(root),
        Box::new(source.clone()),
    )
    .unwrap();
    reopened
        .deliver(wait_of(&request), answer(&request))
        .unwrap();
    let request = parked(reopened.run(&mut host, u64::MAX).unwrap());
    reopened
        .deliver(wait_of(&request), answer(&request))
        .unwrap();
    let Step::Ended(end) = reopened.run(&mut host, u64::MAX).unwrap() else {
        panic!("the run ends");
    };
    assert_eq!(result(end), int(0));

    // Each pass enters the call, then the call its arity refuses.
    let sites = [main_site([1, 1, 0, 0]), main_site([1, 1, 1, 0, 0, 0])];
    let expected: Vec<(EffectIdentity, Site)> = (0..2)
        .flat_map(|visit| {
            sites.clone().map(|site| {
                let identity = EffectIdentity {
                    task: TaskIdentity::Main,
                    site: site.clone(),
                    occurrence: visit,
                    loops: vec![LoopIteration {
                        site: main_site([1]),
                        iteration: visit,
                    }],
                };
                (identity, site)
            })
        })
        .collect();
    let entered: Vec<(EffectIdentity, Site)> = host
        .entered
        .iter()
        .map(|at| (effect_parts(&at.at).clone(), at.origin.clone()))
        .collect();
    assert_eq!(entered, expected);
}

/// K-EFF-008 / K-MACH-008: later observation sees the same future call
/// occurrences and effect identities. A Watch counts only its observed
/// suffix; the task's call counters include the unobserved prefix too.
#[test]
fn observation_attached_after_a_slice_keeps_prior_call_occurrences() {
    use super::embedder::{World, int};
    use crate::{Host, HostFault, ObservationMode, Outcome, Request};
    use lash_kernel_doc::{Datum, ErrorDatum};
    use lash_kernel_value::kernel_internals::effect_parts;
    use lash_kernel_value::sites::{EffectLocation, PositionLabel, Site};

    #[derive(Default)]
    struct Later {
        active: bool,
        world: World,
        entered: Vec<EffectLocation>,
    }
    impl Host for Later {
        fn cancel_requested(&mut self) -> bool {
            self.world.cancel_requested()
        }
        fn observes_sites(&self) -> bool {
            self.active
        }
        fn enter_site(&mut self, at: &EffectLocation) {
            self.entered.push(at.clone());
        }
        fn perform(
            &mut self,
            effect: &lash_kernel_value::OperationUse,
            args: &[Datum],
            site: &PositionLabel,
            origin: &Site,
        ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
            self.world.perform(effect, args, site, origin)
        }
        fn print(&mut self, value: &Datum) {
            self.world.print(value);
        }
    }

    const TEXT: &str = "main { let f = fn(x) { return x } let total = 0 for x in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] { set total = num.add(total, apply f(x)) } do perform 0(total) as Any return total }";
    const NESTED: &str = "main { let f = fn(x) { let answer = call helper.item(null, x) return answer } let total = 0 for x in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] { set total = num.add(total, apply f(x)) } do perform 0(total) as Any return total }";
    for text in [TEXT, NESTED] {
        // Watch attachment and site-observer attachment are independent:
        // the latter run has no Watch at any point.
        for late_watch in [true, false] {
            let run = |late: bool| {
                let mut embedder = Embedder::with(
                    text,
                    Setup {
                        probes: Some(super::layout::helper_probes),
                        ..Setup::default()
                    },
                );
                let mut host = Later {
                    active: late_watch || !late,
                    ..Later::default()
                };
                if late_watch && !late {
                    embedder.machine.set_observe(ObservationMode::Counts);
                }
                loop {
                    assert_eq!(
                        embedder.machine.run(&mut host, 5).unwrap(),
                        Step::Slice,
                        "the prefix ends at a slice before the effect"
                    );
                    if embedder.machine.meters().charged.unwrap() >= 60 {
                        break;
                    }
                }
                let prefix_charge = embedder.machine.meters().charged;
                if !late {
                    assert!(
                        !host.entered.is_empty(),
                        "the prefix entered the repeated call"
                    );
                }
                host.entered.clear();
                embedder.machine.take_counts();
                if late_watch && late {
                    embedder.machine.set_observe(ObservationMode::Counts);
                }
                host.active = true;
                let mut requests = Vec::new();
                let end = loop {
                    match embedder.machine.run(&mut host, u64::MAX).unwrap() {
                        Step::Slice => {}
                        Step::Parked(park) => {
                            for request in park.requests {
                                embedder
                                    .machine
                                    .deliver(
                                        super::embedder::wait_of(&request),
                                        Outcome::Completed(Datum::Null),
                                    )
                                    .unwrap();
                                requests.push(request);
                            }
                        }
                        Step::Ended(end) => break end,
                    }
                };
                assert_eq!(result(end), int(55));
                let last = host.entered.last().expect("the suffix entered calls");
                assert_eq!(effect_parts(&last.at).occurrence, 9);
                let [Request::Effect(effect)] = requests.as_slice() else {
                    panic!("one final effect: {requests:?}");
                };
                assert_eq!(effect.args, vec![int(55)]);
                (
                    prefix_charge,
                    embedder.machine.meters().charged,
                    embedder.machine.take_counts(),
                    host.entered,
                    requests,
                )
            };
            assert_eq!(
                run(true),
                crate::vm::calls::without_memo(|| run(false)),
                "late Watch={late_watch}, {text}"
            );
        }
    }
}
