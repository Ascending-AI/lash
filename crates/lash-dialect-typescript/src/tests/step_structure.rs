//! The prepared form states each statement's step edge: the blocks its step
//! leaves and the Boundary the frame is at then. The machine takes that
//! edge and derives nothing from the frame. The law: on the speed gate's
//! programs and on generated documents, the stated edge is the one the
//! frame shows at the end of every statement's step, derived as the machine
//! once derived it at run time.
//!
//! Each statement runs as one range of the flat stream, and no range spans
//! a Boundary. The second law checks every range of real lowered code: the
//! kernel library with the TypeScript and Python helpers in two layouts,
//! the gate's programs and the generated documents.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use arbitrary::{Arbitrary, Unstructured};
use lash_kernel_conformance::smith::{Smith, compare};
use lash_kernel_dialect::Environment;
use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Name, Timestamp};
use lash_kernel_vm::{
    Bindings, Bounds, DeliverError, Delivered, End, ExportError, Host, ImportError, KernelMachine,
    Layout, Machine, MachineError, Meters, Outcome, PreparedLibrary, Program, Request, Start,
    StartError, Step, Target, WaitId,
};

#[path = "../../shared/gate_six.rs"]
mod gate_six;

const BOUNDS: Bounds = Bounds {
    charge: 1_000_000_000,
    memory: 1 << 30,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct Quiet;

impl Host for Quiet {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "the law's host has no projection".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, _value: &Datum) {}

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

thread_local! {
    /// The statement ends every [`Verified`] machine of this thread has
    /// checked.
    static VERIFIED: Cell<u64> = const { Cell::new(0) };
    /// The statements whose ops every [`Verified`] machine of this thread
    /// has checked.
    static RANGES: Cell<usize> = const { Cell::new(0) };
}

/// Checks the ops of every statement of the document `machine` runs.
fn check_ranges(machine: &KernelMachine) {
    let checked = machine
        .verify_stream()
        .unwrap_or_else(|error| panic!("a lowered range: {error}"));
    RANGES.with(|ranges| ranges.set(ranges.get() + checked));
}

/// A kernel machine that checks its step structure as it runs.
struct Verified(KernelMachine);

impl Machine for Verified {
    type Parked = <KernelMachine as Machine>::Parked;

    fn start(program: Program, bounds: Bounds, start: Start) -> Result<Self, StartError> {
        let mut machine = KernelMachine::start(program, bounds, start)?;
        machine.verify_step_structure();
        check_ranges(&machine);
        Ok(Self(machine))
    }

    fn start_reading(
        program: Program,
        bounds: Bounds,
        start: Start,
        modes: lash_kernel_vm::EffectModes,
    ) -> Result<Self, StartError> {
        let mut machine = KernelMachine::start_reading(program, bounds, start, modes)?;
        machine.verify_step_structure();
        check_ranges(&machine);
        Ok(Self(machine))
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        let before = self.0.verified_steps();
        let step = self.0.run(host, slice);
        let checked = self.0.verified_steps() - before;
        VERIFIED.with(|verified| verified.set(verified.get() + checked));
        step
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        self.0.deliver(wait, outcome)
    }

    fn export(&mut self) -> Result<Self::Parked, ExportError> {
        self.0.export()
    }

    fn import(program: Program, bounds: Bounds, parked: Self::Parked) -> Result<Self, ImportError> {
        let mut machine = KernelMachine::import(program, bounds, parked)?;
        machine.verify_step_structure();
        check_ranges(&machine);
        Ok(Self(machine))
    }

    fn meters(&self) -> Meters {
        self.0.meters()
    }
}

/// Runs a gate program to its end with the step structure checked, giving
/// the statement ends checked.
fn verified_run(name: &str, program: Program) -> u64 {
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    let mut machine =
        KernelMachine::start(program, BOUNDS, start).unwrap_or_else(|error| panic!("{error}"));
    machine.verify_step_structure();
    check_ranges(&machine);
    loop {
        match machine
            .run(&mut Quiet, u64::MAX)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            Step::Parked(park) => {
                for request in park.requests {
                    let (wait, outcome) = match request {
                        Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
                        Request::Effect(effect) => {
                            let argument = effect.args.into_iter().next().unwrap_or(Datum::Null);
                            (
                                effect.wait,
                                gate_six::answer(effect.effect.as_str(), argument),
                            )
                        }
                    };
                    machine
                        .deliver(wait, outcome)
                        .unwrap_or_else(|error| panic!("{name}: {error}"));
                }
            }
            Step::Ended(End::Finished(_)) => return machine.verified_steps(),
            Step::Ended(other) => panic!("{name} ended {other:?}"),
            Step::Slice => unreachable!("the slice is unbounded"),
        }
    }
}

#[test]
fn the_prepared_step_structure_is_the_one_the_frame_shows() {
    let effects = gate_six::effects();
    let tool_roots: BTreeSet<Name> = ["processes", "jobs"].into_iter().map(Name::new).collect();
    let environment = Environment {
        library: super::library(),
        effects: &effects,
        tool_roots: &tool_roots,
        controls: super::controls(),
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    let mut programs = 0;
    let mut gate = 0;
    for (name, source) in gate_six::programs() {
        let lowered = crate::lower(&source, &environment)
            .unwrap_or_else(|diagnostic| panic!("{name}: {diagnostic}"));
        let program = Program {
            document: std::sync::Arc::new(lowered.document),
            library: super::machine::prepared().clone(),
        };
        gate += verified_run(name, program);
        programs += 1;
    }
    // The generated documents run once plainly and once checked, under the
    // same schedule: a checked step that disagrees faults, and the two runs
    // then differ.
    let mut documents = 0;
    for seed in 0..256u64 {
        let mut state = seed + 1;
        let bytes: Vec<u8> = (0..512)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let smith = Smith::arbitrary(&mut Unstructured::new(&bytes))
            .unwrap_or_else(|error| panic!("bounded generation: {error}"));
        compare::<KernelMachine, Verified>(&smith)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        documents += 1;
    }
    let generated = VERIFIED.with(Cell::get);
    assert!(
        gate > 0 && generated > 0,
        "every statement end went unchecked"
    );
    let documents_ranges = RANGES.with(Cell::get);
    assert!(documents_ranges > 0, "no document's ranges were checked");
    println!(
        "step structure: executed programs={programs} gate_steps={gate} documents={documents} generated_steps={generated} document_ranges={documents_ranges}"
    );
}

/// No range of the library's lowered bodies spans a Boundary, with the
/// Python helpers beside the TypeScript ones, in the layout that fuses
/// charges into reads and in one that does not (`K-MACH-008`).
#[test]
fn every_library_range_is_well_formed() {
    let mut registry = (**super::machine::registry()).clone();
    let mut names =
        lash_kernel_dialect::NamedLibrary::from_registry(&registry).expect("unique library names");
    for definition in
        lash_dialect_python::define_helpers(&mut names).unwrap_or_else(|error| panic!("{error}"))
    {
        registry
            .register(definition, None)
            .unwrap_or_else(|error| panic!("{error}"));
    }
    let registry = std::sync::Arc::new(registry);
    let mut checked = Vec::new();
    for layout in [Layout(0), Layout(1), Layout(0x5eed)] {
        let library = PreparedLibrary::with_layout(std::sync::Arc::clone(&registry), layout);
        let ranges = library
            .verify_stream()
            .unwrap_or_else(|error| panic!("layout {layout:?}: {error}"));
        assert!(ranges > 0, "layout {layout:?} checked nothing");
        checked.push(ranges);
    }
    assert!(
        checked.windows(2).all(|pair| pair[0] == pair[1]),
        "every layout lowers the same statements: {checked:?}"
    );
    println!(
        "library ranges: functions={} statements={checked:?}",
        registry.iter().count()
    );
}
