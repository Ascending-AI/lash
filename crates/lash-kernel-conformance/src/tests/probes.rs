//! The r56 probes on the bytecode core: each lowered program at slices 1, 7
//! and 50, against literal prints, end, effects, charge and the charge at
//! every slice return. Where the base is wrong the literal is derived by
//! rule (probes/README.md): steps per `K-MACH-002` (R3, R4), hooks per R2,
//! coverage per `K-CHG-007` and R5, and no slice return without a ready
//! task (`K-MACH-005`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_state::paged::{Checkpoint, PageSource, SealBudget};
use lash_kernel_value::observation::{ExecutionPath, RunObservation, SiteCounts};
use lash_kernel_value::package::{Fill, Link};
use lash_kernel_vm::Weight;
use lash_kernel_vm::{
    Bounds, CellStart, Charge, ChargeTape, CoreMachine, DeliverError, Delivered, Host,
    KernelMachine, Machine, MachineError, MachineOptions, Meters, OpenError, Outcome,
    PreparedLibrary, Program, SealError, SealedRoot, StartError, Step, WaitId,
};

use crate::machine::{Run, run_program_in};
use crate::{Shard, check_program};

const PROBE_FILES: &[(&str, &str)] = &[
    ("r56-c1.json", include_str!("../../probes/clo/r56-c1.json")),
    ("r56-c2.json", include_str!("../../probes/clo/r56-c2.json")),
    ("r56-c3.json", include_str!("../../probes/clo/r56-c3.json")),
    ("r56-c4.json", include_str!("../../probes/clo/r56-c4.json")),
    (
        "r56-c4b.json",
        include_str!("../../probes/clo/r56-c4b.json"),
    ),
    ("r56-c5.json", include_str!("../../probes/clo/r56-c5.json")),
    ("r56-c6.json", include_str!("../../probes/clo/r56-c6.json")),
    (
        "r56-exc1.json",
        include_str!("../../probes/err/r56-exc1.json"),
    ),
    (
        "r56-exc2.json",
        include_str!("../../probes/err/r56-exc2.json"),
    ),
    (
        "r56-exc3.json",
        include_str!("../../probes/err/r56-exc3.json"),
    ),
    (
        "r56-exc4.json",
        include_str!("../../probes/err/r56-exc4.json"),
    ),
    (
        "r56-exc5.json",
        include_str!("../../probes/err/r56-exc5.json"),
    ),
    ("r56-s1.json", include_str!("../../probes/eval/r56-s1.json")),
    (
        "r56-empty.json",
        include_str!("../../probes/fn/r56-empty.json"),
    ),
    ("r56-m1.json", include_str!("../../probes/fn/r56-m1.json")),
    ("r56-m2.json", include_str!("../../probes/fn/r56-m2.json")),
    (
        "r56-e4a.json",
        include_str!("../../probes/form/r56-e4a.json"),
    ),
    (
        "r56-e4b.json",
        include_str!("../../probes/form/r56-e4b.json"),
    ),
    (
        "r56-e4c.json",
        include_str!("../../probes/form/r56-e4c.json"),
    ),
    (
        "r56-e4c_nop.json",
        include_str!("../../probes/form/r56-e4c_nop.json"),
    ),
    (
        "r56-e4d.json",
        include_str!("../../probes/form/r56-e4d.json"),
    ),
    ("r56-i1.json", include_str!("../../probes/iter/r56-i1.json")),
    (
        "r56-i1d.json",
        include_str!("../../probes/iter/r56-i1d.json"),
    ),
    ("r56-i2.json", include_str!("../../probes/iter/r56-i2.json")),
    ("r56-i3.json", include_str!("../../probes/iter/r56-i3.json")),
    ("r56-i4.json", include_str!("../../probes/iter/r56-i4.json")),
    ("r56-i5.json", include_str!("../../probes/iter/r56-i5.json")),
    ("r56-i6.json", include_str!("../../probes/iter/r56-i6.json")),
    ("r56-i7.json", include_str!("../../probes/iter/r56-i7.json")),
    (
        "r56-i7d.json",
        include_str!("../../probes/iter/r56-i7d.json"),
    ),
    ("r56-i8.json", include_str!("../../probes/iter/r56-i8.json")),
    ("r56-k1.json", include_str!("../../probes/key/r56-k1.json")),
    ("r56-b1.json", include_str!("../../probes/num/r56-b1.json")),
    ("r56-f1.json", include_str!("../../probes/num/r56-f1.json")),
    ("r56-f2.json", include_str!("../../probes/num/r56-f2.json")),
    ("r56-t1.json", include_str!("../../probes/task/r56-t1.json")),
    ("r56-t2.json", include_str!("../../probes/task/r56-t2.json")),
    ("r56-t3.json", include_str!("../../probes/task/r56-t3.json")),
    ("r56-t4.json", include_str!("../../probes/task/r56-t4.json")),
    ("r56-u1.json", include_str!("../../probes/text/r56-u1.json")),
];

/// Links `text` with the Python and ECMAScript services filling its helper
/// operations, and a conformance contract for its own imports.
pub(super) fn linked(library: &PreparedLibrary, text: &str) -> Result<Program, String> {
    let document =
        Arc::new(lash_kernel_doc::parse_document(text).map_err(|error| format!("{error:?}"))?);
    let services = || {
        lash_kernel_services::TYPESCRIPT
            .iter()
            .chain(lash_kernel_services::PYTHON.iter())
    };
    let helper_fills = lash_kernel_doc::helper_imports(&document, library.helpers().as_ref())
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(id, import)| {
            let operation = services()
                .find(|operation| operation.name == import.name)
                .ok_or_else(|| format!("unknown helper operation {}", import.name))?;
            Ok((
                id,
                lash_kernel_value::HelperFill {
                    import,
                    fill: Fill::Operation(operation.requirement.clone()),
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                return Err("a probe imports only operations".to_owned());
            };
            let requirement = services()
                .find(|operation| operation.name == import.name)
                .map(|operation| operation.requirement.clone())
                .unwrap_or_else(|| {
                    lash_kernel_value::OperationRequirement::new(
                        signature.clone(),
                        lash_kernel_value::ContractId::new("conformance"),
                    )
                });
            Ok((*slot, Fill::Operation(requirement)))
        })
        .collect::<Result<_, String>>()?;
    let link = Link {
        helper_fills,
        document: document.identity(),
        fills,
    };
    Program::prepare(
        link.identity(),
        std::collections::BTreeMap::from([(document.identity(), document)]),
        &std::collections::BTreeMap::from([(link.identity(), link)]),
        lash_kernel_doc::heap::PageId([7; 32]),
        library.clone(),
        lash_kernel_doc::Selection::Fresh,
    )
    .map_err(|error| error.to_string())
}

#[test]
fn every_r56_probe_holds_on_the_bytecode_core() {
    let release = lash_kernel_releases::helper_release().expect("helper release");
    let library = PreparedLibrary::with_helpers(
        Arc::new(crate::installed_functions().expect("installed functions")),
        Arc::clone(&release.helpers),
    );
    let mut failures = Vec::new();
    for (file, text) in PROBE_FILES {
        let shard: Shard = serde_json::from_str(text).expect(file);
        for case in &shard.cases {
            let checked = linked(&library, &case.document).and_then(|program| {
                check_program::<CoreMachine>(program, case, Charge::Exact)
                    .map(drop)
                    .map_err(|error| error.to_string())
            });
            if let Err(why) = checked {
                failures.push(format!("{file} {}: {why}", case.name));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

thread_local! {
    /// What the observed runs on this thread saw: counts by ordinal, and
    /// the statement path.
    static SEEN: RefCell<(BTreeMap<u32, u64>, ExecutionPath)> = RefCell::default();
    /// The charges the observed run on this thread recorded.
    static TAPE: RefCell<ChargeTape> = RefCell::default();
}

/// A machine that records its charges on a tape.
trait Taped {
    fn record_charges(&mut self, tape: ChargeTape);
}

impl Taped for CoreMachine {
    fn record_charges(&mut self, tape: ChargeTape) {
        CoreMachine::record_charges(self, tape);
    }
}

impl Taped for KernelMachine {
    fn record_charges(&mut self, tape: ChargeTape) {
        KernelMachine::record_charges(self, tape);
    }
}

/// `M` observed: counts and the statement path on from its start, taken
/// at every run return into [`SEEN`], and its charges on [`TAPE`].
struct Watched<M>(M);

impl<M: Machine + Taped> Watched<M> {
    fn watching(mut machine: M) -> Self {
        machine.set_options(MachineOptions {
            observation: RunObservation::Counts,
            trace_path: true,
            ..Default::default()
        });
        machine.record_charges(TAPE.with_borrow(Clone::clone));
        Self(machine)
    }
}

impl<M: Machine + Taped> Machine for Watched<M> {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        M::start(program, bounds, start, source).map(Self::watching)
    }
    fn open(
        program: Program,
        bounds: Bounds,
        root: &Checkpoint,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        M::open(program, bounds, root, source).map(Self)
    }
    fn set_options(&mut self, options: MachineOptions) {
        self.0.set_options(options);
    }
    fn take_counts(&mut self) -> SiteCounts {
        self.0.take_counts()
    }
    fn take_path(&mut self) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.0.take_path()
    }
    fn take_caught(&mut self) -> Vec<WaitId> {
        self.0.take_caught()
    }
    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        let step = self.0.run(host, slice);
        let counts = self.0.take_counts();
        let path = self.0.take_path().expect("the path");
        SEEN.with_borrow_mut(|(seen, held)| {
            for (ordinal, count) in counts.iter() {
                *seen.entry(*ordinal).or_default() += count;
            }
            held.append(path);
        });
        step
    }
    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        self.0.deliver(wait, outcome)
    }
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        self.0.seal(budget)
    }
    fn meters(&self) -> Meters {
        self.0.meters()
    }
}

/// What the last observed run saw, its charges as the events a bound
/// tells apart.
fn seen() -> (
    BTreeMap<u32, u64>,
    lash_kernel_value::observation::ReplayPath,
    Vec<Weight>,
) {
    let (counts, path) = SEEN.take();
    (counts, path.replay(), Weight::expand(TAPE.take().steps()))
}

// Observation changes no result (`observe/`): every probe holds on the
// observed core, and its site counts and per-task statement path are the
// base Source executor's, the oracle for what is seen. Its charge tape
// sums to the probe's literal charge, and is the base's event for event
// where the literal is the base's charge (probes/README.md).
#[test]
fn observing_the_bytecode_core_changes_nothing_and_sees_what_the_base_sees() {
    let release = lash_kernel_releases::helper_release().expect("helper release");
    let library = PreparedLibrary::with_helpers(
        Arc::new(crate::installed_functions().expect("installed functions")),
        Arc::clone(&release.helpers),
    );
    let mut failures = Vec::new();
    for (file, text) in PROBE_FILES {
        let shard: Shard = serde_json::from_str(text).expect(file);
        for case in &shard.cases {
            let program = linked(&library, &case.document).expect(file);
            SEEN.take();
            TAPE.take();
            let observed =
                match check_program::<Watched<CoreMachine>>(program.clone(), case, Charge::Exact) {
                    Ok(observed) => observed,
                    Err(why) => {
                        failures.push(format!("{file} {}: observed: {why}", case.name));
                        continue;
                    }
                };
            let core = seen();
            let taped: u64 = core.2.iter().map(|weight| weight.units()).sum();
            if Some(taped) != observed.charged {
                failures.push(format!(
                    "{file} {}: tape sums to {taped}, charged {:?}",
                    case.name, observed.charged
                ));
            }
            let mut start = |program, bounds, start, source: &crate::pages::Source| {
                <KernelMachine as lash_kernel_vm::Machine>::start(
                    program,
                    bounds,
                    start,
                    Box::new(source.clone()),
                )
                .map(Watched::<KernelMachine>::watching)
            };
            let run = Run {
                start: &mut start,
                printed: &mut || {},
                charge: None,
            };
            let based = match run_program_in(program, case, run, &mut |at| Ok(at.machine), None) {
                Ok(based) => based,
                Err(why) => {
                    failures.push(format!("{file} {}: base: {why}", case.name));
                    continue;
                }
            };
            let base = seen();
            if based.charged == observed.charged && core.2 != base.2 {
                let at = core
                    .2
                    .iter()
                    .zip(&base.2)
                    .take_while(|(a, b)| a == b)
                    .count();
                failures.push(format!(
                    "{file} {}: tape != base at event {at} of {}/{}: {:?} != {:?}",
                    case.name,
                    core.2.len(),
                    base.2.len(),
                    &core.2[at.saturating_sub(3)..(at + 4).min(core.2.len())],
                    &base.2[at.saturating_sub(3)..(at + 4).min(base.2.len())],
                ));
            }
            if core.0 != base.0 {
                failures.push(format!(
                    "{file} {}: counts {:?} != base {:?}",
                    case.name, core.0, base.0
                ));
            }
            if core.1 != base.1 {
                failures.push(format!(
                    "{file} {}: path {:?} != base {:?}",
                    case.name, core.1, base.1
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
