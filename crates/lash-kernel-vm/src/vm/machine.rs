//! The bytecode core behind the machine interface ([`crate::Machine`]):
//! each start compiles its own [`Program`](crate::bytecode::program::Program)
//! and runs it on a slot heap.
//!
//! A seal writes the run's state as a site-keyed run record
//! ([`crate::durable`]) under the checkpoint root; an open recompiles the
//! document as its start did and restores the record into the new build.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lash_kernel_doc::{Datum, Decoder, Document, Value};
use lash_kernel_state::paged::run::continuation;
use lash_kernel_state::paged::{
    CellTargetKind, Checkpoint, CheckpointRoot, KeyedRoot, PageKind, PageSource, RootStatus,
    SavedLimits, SavedMeters, SealBudget, SequenceRoot,
};
use lash_kernel_value::names::{FunctionName, Name};
use lash_kernel_value::package::OperationMode;
use lash_kernel_value::tokens::encode_body;
use lash_kernel_value::{HelperOperationId, OperationSlot};

use super::tasks::{Operation, Operations};
use crate::bytecode::program::ActionSite;

use super::watch::Watch;
use super::{Stop, Vm, value_depth};
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
use crate::interface::{
    BoundExceeded, Bounds, Charge, DeliverError, Delivered, End, Host, Meters, Outcome, Program,
    RunError, StartError, Step, Target, WaitId,
};
use crate::paged::{CellStart, Finished, Machine, MachineError, OpenError, SealError, SealedRoot};
use crate::{ChargeTape, ExecutionPath, MachineOptions, SiteCounts};

/// One run of one document on the bytecode core.
pub struct CoreMachine {
    vm: Vm<SlotHeap>,
    document: Arc<Document>,
    ended: bool,
    convention: Option<lash_kernel_doc::StartConvention>,
    header: Header,
    /// A budgeted seal's written run record and what the checkpoint root
    /// takes from it, until the run moves.
    partial: Option<Partial>,
}

/// The run record a budgeted seal wrote before its budget ran out.
struct Partial {
    heap: lash_kernel_doc::heap::PageRef,
    meters: SavedMeters,
}

/// What every root the run seals names besides its state: fixed at start.
struct Header {
    bounds: Bounds,
    kernel: u32,
    document: lash_kernel_value::names::DocumentId,
    link: lash_kernel_value::content_ids::LinkId,
    closure: lash_kernel_doc::heap::PageId,
    helper_library: lash_kernel_value::HelperLibraryId,
    owner: lash_kernel_doc::heap::PageId,
    host: Vec<Name>,
    /// The export the run started on; `main` when none.
    entry: Option<Name>,
}

impl Header {
    fn of(program: &Program, bounds: Bounds, owner: lash_kernel_doc::heap::PageId) -> Self {
        Self {
            bounds,
            kernel: program.document.kernel(),
            document: program.library.document_identity(&program.document),
            link: program.link,
            closure: program.closure,
            helper_library: program.helpers().identity(),
            owner,
            host: Vec::new(),
            entry: None,
        }
    }
}

/// The run of `program` on `entry` (`main` when none) compiled as a start
/// compiles it, over `heap`, having charged `used`.
fn build(
    program: &Program,
    bounds: Bounds,
    entry: Option<&Name>,
    modes: crate::interface::EffectModes,
    heap: SlotHeap,
    used: u64,
    available: Option<&lash_kernel_doc::BuildHelpers>,
) -> Result<(Vm<SlotHeap>, Option<lash_kernel_doc::StartConvention>), StartError> {
    let document = &program.document;
    let export = match entry {
        None => None,
        Some(name) => match document.exports.get(name) {
            Some(lash_kernel_doc::Export::Function(export)) => Some(export),
            _ => {
                return Err(StartError::UnknownEntry {
                    entry: name.clone(),
                });
            }
        },
    };
    let convention = export.map(|export| export.convention.clone());
    let registry = program.library.registry().clone();
    let mut code = crate::bytecode::conventions::compile_selected(
        document,
        &registry,
        program.link,
        export.map(|export| export.function),
        convention.as_ref(),
        available,
    )
    .map_err(linked)?;
    crate::bytecode::inline::inline(&mut code);
    let heights = crate::bytecode::validate::validate(&code).map_err(linked)?;
    crate::bytecode::fold::fold(&mut code);
    crate::bytecode::fuse::fuse(&mut code);
    let paced = matches!(bounds.charge, Charge::Off);
    let operations = operations(program, &code, modes)?;
    let mut vm = Vm::new(
        Arc::clone(document),
        Arc::new(code),
        &heights,
        heap,
        used,
        bounds.charge_limit,
        paced,
        bounds.call_depth,
    )
    .map_err(|stop| linked(format!("{stop:?}")))?;
    vm.tasks.operations = operations;
    vm.tasks.live_limit = bounds.live_tasks;
    vm.tasks.requests_limit = bounds.requests_per_park;
    vm.tasks.join_limit = bounds.join_members;
    Ok((vm, convention))
}

/// A build helper the run's live code reaches and the opening image lacks.
fn unavailable_helper(function: lash_kernel_value::FunctionId) -> OpenError {
    OpenError::HelperUnavailable {
        reason: lash_kernel_value::HelperUnavailableReason::Unknown,
        function,
    }
}

fn malformed(problem: impl ToString) -> OpenError {
    OpenError::Malformed {
        problem: problem.to_string(),
    }
}

fn linked(problem: impl ToString) -> StartError {
    StartError::Linked {
        problem: problem.to_string(),
    }
}

impl CoreMachine {
    /// Seals the heap and makes the run record.
    fn seal_heap(&mut self) -> Result<continuation::RunRecord, SealError> {
        let mut reads: Vec<_> = self
            .vm
            .tasks
            .operations
            .values()
            .filter_map(|operation| operation.as_ref().ok())
            .filter(|operation| operation.read)
            .map(|operation| operation.use_)
            .collect();
        reads.sort();
        reads.dedup();
        crate::durable::seal(&mut self.vm, self.header.entry.clone(), reads)
    }

    /// Records every charge the run makes from here on in `tape`, in order
    /// (`K-CHG-002`); recording changes no result, charge or slice return.
    pub fn record_charges(&mut self, tape: ChargeTape) {
        match self.vm.watch.as_deref_mut() {
            Some(watch) => watch.tape = Some(tape),
            None => {
                self.vm.watch =
                    Watch::new(&self.document, &self.vm.program, false, false, Some(tape))
                        .map(Box::new);
            }
        }
    }

    /// Pages every object the heap can out at each statement boundary:
    /// the laws' forced-cold runs.
    #[cfg(test)]
    pub(crate) fn force_cold(&mut self) {
        self.vm.chill = Some(|heap, roots| {
            heap.page_out(roots)
                .expect("a forced-cold run seals its heap");
        });
    }

    /// Builds `datum` into the run's heap, every mutable object fresh.
    /// An input past the memory bound is held for the first step, as a
    /// refused program or constant is (`K-BND-001`).
    fn copy_in(vm: &mut Vm<SlotHeap>, datum: &Datum) -> Result<Value, StartError> {
        if vm.unadmitted() {
            return Ok(Value::Absent);
        }
        let built = super::datum::copy_in(vm, datum);
        vm.admitted(built, Value::Absent)
            .map_err(|flow| linked(format!("{flow:?}")))
    }

    fn ended(&mut self, end: End) -> Result<Step, MachineError> {
        self.ended = true;
        Ok(Step::Ended(end))
    }

    fn exceeded(exceeded: super::Exceeded) -> End {
        End::Error(RunError::Bound(BoundExceeded {
            bound: exceeded.bound,
            limit: exceeded.limit,
            function: exceeded
                .function
                .and_then(|name| FunctionName::new(name.to_string()).ok()),
            site: exceeded.site.map(Box::new),
        }))
    }
}

impl Machine for CoreMachine {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        if start.session.is_some() {
            return Err(StartError::Bindings {
                problem: "the bytecode core opens no session before the durable heap lands".into(),
            });
        }
        let document = program.document.clone();
        let entry = match &start.target {
            Target::Main => None,
            Target::Entry(name) => Some(name.clone()),
            Target::Callable(_) => {
                return Err(linked("the bytecode core starts no callable yet"));
            }
        };
        let export = entry
            .as_ref()
            .and_then(|name| match document.exports.get(name) {
                Some(lash_kernel_doc::Export::Function(export)) => Some(export),
                _ => None,
            });
        let params = export.map_or_else(Vec::new, |export| export.signature.params.clone());
        let heap = SlotHeap::durable(
            bounds.memory,
            crate::heap::open::Durable {
                directory: lash_kernel_state::paged::run::directory::Directory::new(source),
                high_water: lash_kernel_state::paged::run::ids::HighWater(0),
            },
        );
        let (mut vm, convention) =
            build(&program, bounds, entry.as_ref(), start.modes, heap, 0, None)?;
        let mut header = Header::of(&program, bounds, start.owner);
        header.host = start.host.iter().map(|(name, _)| name.clone()).collect();
        header.entry = entry;
        let decoder = Decoder {
            max_depth: value_depth(),
            policy: document.numbers,
            declared: &|callable| document.functions.contains_key(&callable.function),
        };
        let mut args = Vec::with_capacity(params.len());
        if let Some(decoded) =
            crate::bytecode::conventions::arguments(&document, export, &start.args)?
        {
            for datum in &decoded {
                args.push(Self::copy_in(&mut vm, datum)?);
            }
        } else {
            for (at, param) in params.iter().enumerate() {
                let value = match start.args.get(at) {
                    Some(datum) => {
                        let decoded = decoder
                            .decode(datum, &param.ty)
                            .map_err(StartError::Decode)?;
                        Self::copy_in(&mut vm, &decoded)?
                    }
                    None if param.optional => Value::Absent,
                    None => {
                        return Err(StartError::MissingArgument {
                            name: param.name.clone(),
                        });
                    }
                };
                args.push(value);
            }
        }
        let globals: Vec<Name> = vm.program.globals.clone();
        for (at, name) in globals.iter().enumerate() {
            if let Some((_, datum)) = start.host.iter().find(|(bound, _)| bound == name) {
                vm.globals[at] = Self::copy_in(&mut vm, datum)?;
            }
        }
        let main = vm.program.main;
        vm.start(main, &args, None)
            .map_err(|flow| linked(format!("{flow:?}")))?;
        Ok(Self {
            vm,
            document,
            ended: false,
            partial: None,
            convention,
            header,
        })
    }

    fn open(
        program: Program,
        bounds: Bounds,
        checkpoint: &Checkpoint,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        crate::paged::admit_format(checkpoint)?;
        let root = lash_kernel_state::kernel_internals::checkpoint_root(checkpoint);
        // The run keeps the limits and charge policy it was sealed with.
        let bounds = Bounds {
            memory_meter: match root.limits.memory {
                Some(_) => crate::MemoryMeter::On,
                None => crate::MemoryMeter::Off,
            },
            memory: root.limits.memory.unwrap_or(bounds.memory),
            charge: root.meters.charge(),
            charge_limit: root.limits.charge,
            ..bounds
        };
        let mut header = Header::of(&program, bounds, root.owner);
        if root.document != header.document {
            return Err(OpenError::DocumentMismatch {
                parked: root.document,
                given: header.document,
            });
        }
        if root.kernel != header.kernel {
            return Err(OpenError::KernelVersion {
                parked: root.kernel,
                supported: header.kernel,
            });
        }
        if root.link != header.link {
            return Err(malformed(
                "the checkpoint's Link differs from the program's",
            ));
        }
        if root.closure != header.closure {
            return Err(malformed(
                "the document's execution closure differs from the checkpoint's",
            ));
        }
        if !matches!(root.status, RootStatus::Parked) {
            return Err(malformed("an ended root cannot execute"));
        }
        let (record, directory) = continuation::open(source, root.heap).map_err(OpenError::Heap)?;
        // Under another build image, the run opens only if the image holds
        // every helper its live resume closure pins (contract §7).
        let changed = (root.helper_library != header.helper_library).then(|| program.helpers());
        if let Some(helpers) = changed {
            // A frame standing in a helper's body names it by its site.
            let framed = record
                .tasks
                .iter()
                .flat_map(|task| &task.frames)
                .filter_map(|frame| match frame.unit.site.unit {
                    lash_kernel_value::sites::Unit::Library(function) => Some(function),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let unavailable = record
                .pins
                .iter()
                .chain(&record.held_code)
                .chain(&framed)
                .find(|function| {
                    !helpers.contains(function)
                        && !program.document.functions.contains_key(function)
                });
            if let Some(function) = unavailable {
                return Err(unavailable_helper(*function));
            }
        }
        if matches!(root.target, CellTargetKind::Main) != record.entry.is_none() {
            return Err(malformed("the root's target and its run's entry disagree"));
        }
        header.host = root.host.clone();
        header.entry = record.entry.clone();
        let (heap, values) = SlotHeap::open(bounds.memory, &record.heap, directory)
            .map_err(|error| malformed(format!("the run's heap does not open: {error:?}")))?;
        let modes = record
            .reads
            .iter()
            .map(|use_| (*use_, OperationMode::Read))
            .collect();
        let (mut vm, convention) = build(
            &program,
            bounds,
            record.entry.as_ref(),
            modes,
            heap,
            record.charged,
            changed.map(|helpers| helpers.as_ref()),
        )
        .map_err(|error| malformed(format!("the document does not compile again: {error:?}")))?;
        crate::durable::admit(&vm.program, &vm.tasks.operations, &record)?;
        crate::durable::open(&mut vm, &record, &values).map_err(malformed)?;
        if changed.is_some() {
            crate::durable::live(&vm).map_err(unavailable_helper)?;
        }
        Ok(Self {
            vm,
            document: Arc::clone(&program.document),
            ended: false,
            partial: None,
            convention,
            header,
        })
    }

    fn set_options(&mut self, options: MachineOptions) {
        self.vm.limiter = options.charge_limiter;
        let tape = self.vm.watch.take().and_then(|watch| watch.tape);
        self.vm.watch = Watch::new(
            &self.document,
            &self.vm.program,
            !options.observation.is_off(),
            options.trace_path,
            tape,
        )
        .map(Box::new);
    }

    fn take_counts(&mut self) -> SiteCounts {
        self.vm
            .watch
            .as_deref_mut()
            .map(Watch::take_counts)
            .unwrap_or_default()
    }

    fn take_path(&mut self) -> Result<ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.vm
            .watch
            .as_deref_mut()
            .map_or_else(|| Ok(ExecutionPath::default()), Watch::take_path)
    }

    fn take_caught(&mut self) -> Vec<WaitId> {
        Vec::new()
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        self.partial = None;
        match self.vm.run(host, slice) {
            Stop::Returned(result) => self.concluded(
                None,
                |result| {
                    End::Finished(Box::new(Finished {
                        result,
                        finish: false,
                        carry: None,
                        not_carried: Vec::new(),
                    }))
                },
                crate::bytecode::conventions::result(self.convention.as_ref(), result),
            ),
            Stop::Finish(result) => {
                let ender = Some(self.vm.tasks.running);
                self.concluded(
                    ender,
                    |result| {
                        End::Finished(Box::new(Finished {
                            result,
                            finish: true,
                            carry: None,
                            not_carried: Vec::new(),
                        }))
                    },
                    result,
                )
            }
            Stop::Fail(datum) => {
                let ender = Some(self.vm.tasks.running);
                self.concluded(ender, End::Failed, datum)
            }
            Stop::Park(park) => Ok(Step::Parked(park)),
            Stop::Deadlock(waiting) => self.ended(End::Error(RunError::Deadlock { waiting })),
            Stop::Cancelled => self.ended(End::Cancelled),
            Stop::Host(fault) => {
                self.ended = true;
                Err(MachineError::Host(fault))
            }
            Stop::Uncaught(value) => self.uncaught(value),
            Stop::Slice => {
                if host.cancel_requested() {
                    return self.ended(End::Cancelled);
                }
                Ok(Step::Slice)
            }
            Stop::Bound(exceeded) => self.ended(Self::exceeded(*exceeded)),
            Stop::Fault(problem) => {
                self.ended = true;
                Err(MachineError::Fault { problem })
            }
        }
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.ended {
            return Err(DeliverError::Ended);
        }
        self.partial = None;
        let n = lash_kernel_doc::kernel_internals::wait_number(wait);
        let held = crate::heap::meter::outcome(&outcome);
        let delivered = self.vm.tasks.deliver(n, outcome)?;
        if delivered == Delivered::Accepted {
            self.vm.hold_raw(held);
        }
        Ok(delivered)
    }

    /// Seals in three phases: the heap, the run record, the checkpoint
    /// root. A phase started finishes, and what it wrote is kept: a heap
    /// sealed is clean, and a record written is reused until the run
    /// moves. Once a phase that wrote pages has spent `budget`, the seal
    /// yields `None`; every call finishes a phase, so calling again
    /// produces the root.
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        if self.ended {
            return Err(SealError::Ended);
        }
        let written = |machine: &Self| {
            machine.vm.heap.access().map_or(
                (0, 0),
                lash_kernel_state::kernel_internals::tree::Access::written,
            )
        };
        let start = written(self);
        let spent = |now: (u64, u64), before: (u64, u64)| {
            now.0 > before.0 && (now.0 - start.0 >= budget.pages || now.1 - start.1 >= budget.bytes)
        };
        let Partial { heap, meters } = match self.partial.take() {
            Some(partial) => partial,
            None => {
                let record = self.seal_heap()?;
                let sealed = written(self);
                if spent(sealed, start) {
                    return Ok(None);
                }
                let access = self.vm.heap.access().ok_or_else(|| SealError::Fault {
                    problem: "the heap has no directory".into(),
                })?;
                let heap = continuation::write(access, &record).map_err(SealError::Heap)?;
                let meters = SavedMeters {
                    charged: (self.header.bounds.charge == Charge::Exact).then_some(record.charged),
                    waits_issued: record.next_wait,
                    next_join: record.next_join,
                    live_tasks: record.live,
                };
                if spent(written(self), sealed) {
                    self.partial = Some(Partial { heap, meters });
                    return Ok(None);
                }
                Partial { heap, meters }
            }
        };
        let access = self.vm.heap.access().ok_or_else(|| SealError::Fault {
            problem: "the heap has no directory".into(),
        })?;
        let header = &self.header;
        let root = CheckpointRoot {
            limits: SavedLimits {
                memory: (header.bounds.memory_meter == crate::MemoryMeter::On)
                    .then_some(header.bounds.memory),
                charge: header.bounds.charge_limit,
            },
            kernel: header.kernel,
            document: header.document,
            link: header.link,
            closure: header.closure,
            helper_library: header.helper_library,
            owner: header.owner,
            target: match header.entry {
                None => CellTargetKind::Main,
                Some(_) => CellTargetKind::Entry,
            },
            functions: KeyedRoot::Empty,
            status: RootStatus::Parked,
            heap,
            tasks: KeyedRoot::Empty,
            ready: SequenceRoot::Empty,
            waits: KeyedRoot::Empty,
            joins: KeyedRoot::Empty,
            withdrawn: KeyedRoot::Empty,
            unreported: SequenceRoot::Empty,
            reads: KeyedRoot::Empty,
            session: KeyedRoot::Empty,
            host: header.host.clone(),
            meters,
        };
        let tokens = encode_body(&root);
        if tokens.len() > 4096 {
            return Err(SealError::Fault {
                problem: "checkpoint header exceeds the root bound".into(),
            });
        }
        let page = access
            .seal(PageKind::CHECKPOINT_ROOT, tokens, vec![heap])
            .map_err(SealError::Heap)?;
        Ok(Some(SealedRoot { page, root }))
    }

    fn meters(&self) -> Meters {
        Meters {
            charged: (!self.vm.paced()).then(|| self.vm.charged()),
            memory: self.vm.heap.held(),
            live_tasks: self.vm.tasks.live,
        }
    }
}

impl CoreMachine {
    /// The run reached its end: an error when tasks other than `main` and
    /// `ender` are outstanding (`K-TASK-018`), else `end` of `result`.
    fn concluded(
        &mut self,
        ender: Option<usize>,
        end: impl FnOnce(Datum) -> End,
        result: Datum,
    ) -> Result<Step, MachineError> {
        match self.vm.tasks.outstanding(ender, &self.vm.program.units) {
            Some(error) => self.ended(End::Error(error)),
            None => self.ended(end(result)),
        }
    }

    fn uncaught(&mut self, value: Value) -> Result<Step, MachineError> {
        match self.vm.copy_stopped(value) {
            Ok(error) => self.ended(End::Error(RunError::Uncaught { error, site: None })),
            Err(error) => {
                self.ended = true;
                Err(MachineError::Fault {
                    problem: format!("an uncaught value does not copy out: {error}"),
                })
            }
        }
    }
}

/// Resolves every `perform`'s operation in `code` against the program's
/// Link, and which the run binds as reads (`K-EFF-012`).
fn operations(
    program: &Program,
    code: &crate::bytecode::program::Program,
    modes: crate::interface::EffectModes,
) -> Result<Operations, StartError> {
    let mut reads = HashSet::new();
    for (use_, mode) in modes {
        let declared = program
            .instances
            .get(&use_.link)
            .and_then(|instance| instance.operation(use_.slot));
        if !matches!(declared, Some(found) if found == use_) {
            return Err(StartError::UndeclaredRead { use_ });
        }
        if mode == OperationMode::Read {
            reads.insert(use_);
        }
    }
    let link = program.link;
    let instance = program.instances.get(&link);
    let document = &program.document;
    let mut operations = HashMap::new();
    for unit in &code.units {
        for action in &unit.actions {
            let ActionSite::Perform { owner, slot, .. } = action else {
                continue;
            };
            operations.entry((*owner, *slot)).or_insert_with(|| {
                let instance = instance.ok_or("the program's Link is not loaded")?;
                let key = match owner {
                    Some(function) if !document.functions.contains_key(function) => {
                        let registered = program
                            .helpers()
                            .registered(function)
                            .or_else(|| program.library.registry().get(function))
                            .ok_or("helper operation body is not loaded")?;
                        let import = registered
                            .imports()
                            .get(slot)
                            .ok_or("helper does not declare this operation")?;
                        OperationSlot::Helper(HelperOperationId::of_import(import))
                    }
                    _ => (*slot).into(),
                };
                let use_ = instance
                    .operation(key)
                    .filter(|use_| use_.link == link && use_.slot == key)
                    .ok_or("perform has no explicit operation fill in its defining Link")?;
                Ok(Operation {
                    read: reads.contains(&use_),
                    use_,
                })
            });
        }
    }
    Ok(operations)
}
