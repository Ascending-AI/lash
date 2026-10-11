//! FIG-5916: the gate-six baseline starts with at most 1,000 allocations
//! after its immutable document has been admitted and prepared.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{EffectName, Name};
use lash_kernel_vm::{
    Bindings, Bounds, KernelMachine, Machine, PreparedLibrary, Program, Start, Target,
};

#[expect(
    dead_code,
    reason = "this allocation law uses the shared sources and effects, not scripted outcomes"
)]
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
    assert!(counts.allocations <= 1_000, "baseline start: {counts:?}");
    assert_eq!(machine.meters().charged, 0);
}
