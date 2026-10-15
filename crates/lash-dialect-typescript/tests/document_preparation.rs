//! FIG-5916: the gate-six baseline starts with at most 1,000 allocations
//! after its immutable document has been admitted and prepared.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{EffectName, Name};
use lash_kernel_vm::{
    Bindings, Bounds, KernelMachine, Machine, PreparedLibrary, Program, Start, Target,
};

#[path = "../shared/gate_six.rs"]
mod gate_six;

#[path = "../shared/gate_six_registry.rs"]
mod gate_six_registry;

#[global_allocator]
static ALLOCATOR: &stats_alloc::StatsAlloc<std::alloc::System> = &stats_alloc::INSTRUMENTED_SYSTEM;

#[test]
fn an_admitted_baseline_cell_starts_with_at_most_one_thousand_allocations() {
    let registry = gate_six_registry::registry();
    let library = NamedLibrary::from_registry(&registry).unwrap();
    let effects = gate_six::effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").unwrap(),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let tool_roots = ["processes", "jobs"].into_iter().map(Name::new).collect();
    let environment = Environment {
        library: &library,
        effects: &effects,
        controls: &controls,
        tool_roots: &tool_roots,
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    let (_, source) = gate_six::programs()
        .into_iter()
        .find(|(name, _)| *name == "baseline")
        .unwrap();
    let program = Program {
        document: Arc::new(
            lash_dialect_typescript::lower(&source, &environment)
                .unwrap()
                .document,
        ),
        library: PreparedLibrary::new(registry),
    };
    let bounds = Bounds {
        charge: 1_000_000_000,
        memory: 1 << 30,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    };
    let start = || Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    drop(KernelMachine::start(program.clone(), bounds, start()).unwrap());
    let counts = stats_alloc::Region::new(ALLOCATOR);
    let machine = KernelMachine::start(program, bounds, start()).unwrap();
    let counts = counts.change();
    eprintln!("baseline start: {counts:?}");
    assert!(counts.allocations <= 1_000, "baseline start: {counts:?}");
    assert_eq!(machine.meters().charged, 0);
}

/// FIG-6369 (e): gate six's document starts agree with an independent live
/// statement trace at unlimited, one-charge and zero-charge slices.
#[test]
fn gate_six_counts_equal_statement_trace_at_every_slice_geometry() {
    use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Timestamp};
    use lash_kernel_vm::{End, Host, Observe, Outcome, Request, Step};
    struct Console;
    impl Host for Console {
        fn clock(&mut self) -> Timestamp {
            Timestamp {
                nanoseconds: Integer::from(0),
            }
        }
        fn random(&mut self) -> u64 {
            0
        }
        fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
            unreachable!()
        }
        fn print(&mut self, _: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let registry = gate_six_registry::registry();
    let library = NamedLibrary::from_registry(&registry).unwrap();
    let prepared = PreparedLibrary::new(registry);
    let effects = gate_six::effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").unwrap(),
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let environment = Environment {
        library: &library,
        effects: &effects,
        controls: &controls,
        tool_roots: &["processes", "jobs"].into_iter().map(Name::new).collect(),
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    for (name, source) in gate_six::programs() {
        let program = Program {
            document: Arc::new(
                lash_dialect_typescript::lower(&source, &environment)
                    .unwrap()
                    .document,
            ),
            library: prepared.clone(),
        };
        let ordinals = program.document.site_ordinals();
        let mut natural = None;
        for slice in [u64::MAX, 1, 0] {
            let mut machine = KernelMachine::start(
                program.clone(),
                Bounds {
                    charge: 1_000_000_000,
                    memory: 1 << 30,
                    call_depth: 200,
                    live_tasks: 100,
                    requests_per_park: 100,
                    join_members: 100,
                },
                Start {
                    target: Target::Main,
                    args: Vec::new(),
                    bindings: Bindings::default(),
                },
            )
            .unwrap();
            machine.set_observe(Observe::Counts);
            lash_kernel_vm::perf_witness::trace_statements(&mut machine);
            let mut total = BTreeMap::<u32, u64>::new();
            loop {
                let step = machine.run(&mut Console, slice).unwrap();
                let delta: BTreeMap<_, _> = machine.take_ran().into_iter().collect();
                let mut trace = BTreeMap::<u32, u64>::new();
                for site in lash_kernel_vm::perf_witness::take_statement_trace(&mut machine) {
                    *trace.entry(ordinals[&site]).or_default() += 1;
                }
                assert_eq!(delta, trace, "{name}, slice {slice}");
                for (ordinal, count) in delta {
                    *total.entry(ordinal).or_default() += count;
                }
                match step {
                    Step::Ended(End::Finished(_)) => break,
                    Step::Ended(end) => panic!("{name}: {end:?}"),
                    Step::Slice => {}
                    Step::Parked(park) => {
                        for request in park.requests {
                            let (wait, answer) = match request {
                                Request::Effect(request) => (
                                    request.wait,
                                    gate_six::answer(
                                        request.effect.as_str(),
                                        request.args.into_iter().next().unwrap_or(Datum::Null),
                                    ),
                                ),
                                Request::Sleep(request) => (request.wait, Outcome::Elapsed),
                            };
                            machine.deliver(wait, answer).unwrap();
                        }
                    }
                }
            }
            if let Some(natural) = &natural {
                assert_eq!(&total, natural, "{name}, slice {slice}");
            } else {
                natural = Some(total);
            }
        }
    }
}
