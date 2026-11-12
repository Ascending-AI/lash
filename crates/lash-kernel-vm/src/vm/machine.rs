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
use lash_kernel_state::paged::SessionRoot;
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
use super::{Stop, Vm};
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
    /// The public result an entry's or callable's value fits as it
    /// leaves the run.
    output: Option<Output>,
    header: Header,
    /// A budgeted seal's written run record and what the checkpoint root
    /// takes from it, until the run moves.
    partial: Option<Partial>,
    /// What saves the functions a session cell's end drops (`K-SES-004`).
    save: Option<Box<dyn crate::CellSave>>,
}

/// The public result a start's value fits as it leaves the run
/// (FIG-7143): a value that does not is the entry's typed mismatch at
/// `output`, uncaught. A TypeScript start's value is its settlement
/// hook's, and fits there.
struct Output {
    callable: lash_kernel_value::CallableId,
    result: lash_kernel_value::Type,
}

impl Output {
    fn of(
        link: lash_kernel_value::LinkId,
        export: &lash_kernel_doc::FunctionExport,
    ) -> Option<Self> {
        let result = &export.signature.result;
        if *result == lash_kernel_value::Type::Any
            || crate::bytecode::conventions::typescript(&export.convention)
        {
            return None;
        }
        Some(Self {
            callable: lash_kernel_value::CallableId {
                link,
                function: export.function,
            },
            result: result.clone(),
        })
    }
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
    /// What the run started on.
    start: continuation::RunStart,
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
            start: continuation::RunStart::Main,
        }
    }
}

/// A heap a run opens over, and the values its root holds.
struct Opened<'a> {
    heap: SlotHeap,
    roots: &'a [Value],
    /// The constants a sealed run placed.
    saved: super::library::Saved,
}

/// The closures of Links other than `link` that `roots` reach through
/// closures and the cells they capture, one for each callable: code only
/// its own Link's document holds, so the build compiles it there. A cold
/// closure or cell is read from its saved row and stays cold: the open
/// holds no more than the run it continues (contract §2, R10).
fn foreign_closures(
    heap: &SlotHeap,
    roots: &[Value],
    link: lash_kernel_value::LinkId,
) -> Result<Vec<lash_kernel_doc::arena::ClosureIdentity>, StartError> {
    use crate::heap::open;
    use lash_kernel_doc::arena::ArenaHandle;
    use lash_kernel_state::paged::run::ids::ObjectId;
    use lash_kernel_state::paged::run::objects::{ObjectKind, Record, RunValue};
    /// A closure or cell of the walk: resident in the arena, or only its
    /// saved row.
    #[derive(Clone, Copy)]
    enum At {
        Held(ArenaHandle),
        Saved(ObjectId),
    }
    let arena = heap.arena();
    let held = |handle: ArenaHandle| -> Result<At, StartError> {
        if arena.facts(handle).map_err(linked)?.cold {
            Ok(At::Saved(open::id_of(arena, handle).map_err(linked)?))
        } else {
            Ok(At::Held(handle))
        }
    };
    let saved = |id: ObjectId| -> Result<At, StartError> {
        match arena.entity_handle(open::entity(id)) {
            Some(handle) => held(handle),
            None => Ok(At::Saved(id)),
        }
    };
    let row = |id: ObjectId| -> Result<Record, StartError> {
        let directory = heap
            .directory()
            .ok_or_else(|| linked("a cold closure in a heap with no directory"))?;
        directory
            .get(id)
            .map_err(linked)?
            .map(|row| row.record)
            .ok_or_else(|| linked(format!("a held closure's object {} has no row", id.0)))
    };
    let mut work = Vec::new();
    for value in roots {
        if let Value::Closure(handle) = value {
            work.push(held(*handle)?);
        }
    }
    let mut seen = HashSet::new();
    let mut callables = HashSet::new();
    let mut found = Vec::new();
    while let Some(at) = work.pop() {
        let key = match at {
            At::Held(handle) => (false, u64::from(handle.index())),
            At::Saved(id) => (true, id.0),
        };
        if !seen.insert(key) {
            continue;
        }
        let (identity, cells) = match at {
            At::Held(handle) => {
                let (identity, captures, _) = arena.closure(handle).map_err(linked)?;
                let cells = captures
                    .iter()
                    .map(|cell| held(*cell))
                    .collect::<Result<Vec<_>, _>>()?;
                (identity.clone(), cells)
            }
            At::Saved(id) => match row(id)? {
                Record::Closure { code, captures } => (
                    lash_kernel_doc::arena::ClosureIdentity {
                        capture_names: code.capture_names.clone(),
                        callable: code.callable,
                        site: code.linked_site(),
                    },
                    captures
                        .iter()
                        .map(|cell| saved(*cell))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                _ => return Err(linked(format!("object {} is not a closure", id.0))),
            },
        };
        for cell in cells {
            let inner = match cell {
                At::Held(handle) => match arena.cell(handle).map_err(linked)? {
                    Value::Closure(inner) => Some(held(inner)?),
                    _ => None,
                },
                At::Saved(id) => match row(id)? {
                    Record::Cell(RunValue::Object(ObjectKind::Closure, inner)) => {
                        Some(saved(inner)?)
                    }
                    Record::Cell(_) => None,
                    _ => return Err(linked(format!("object {} is not a cell", id.0))),
                },
            };
            work.extend(inner);
        }
        if identity.callable.link != link && callables.insert(identity.callable) {
            found.push(identity);
        }
    }
    Ok(found)
}

/// The run of `program` on `entry` (`main` when none) compiled as a start
/// compiles it, over `opened`, having charged `used`.
fn build(
    program: &Program,
    bounds: Bounds,
    root: Option<&super::callable::Root<'_>>,
    modes: crate::interface::EffectModes,
    opened: Opened<'_>,
    used: u64,
    available: Option<&lash_kernel_doc::BuildHelpers>,
) -> Result<(Vm<SlotHeap>, Option<lash_kernel_doc::StartConvention>), StartError> {
    let document = &program.document;
    let convention = root.map(|root| root.entry.convention.clone());
    let registry = program.library.registry().clone();
    // Every Link the build loads: a function value of one calls that
    // Link's own body.
    let others = program
        .instances
        .iter()
        .filter(|(link, _)| **link != program.link)
        .filter_map(|(link, instance)| {
            let document = program.documents.get(&instance.document)?;
            Some((*link, &**document))
        })
        .collect();
    let imports = program
        .instances
        .iter()
        .flat_map(|(link, instance)| {
            instance
                .slots
                .iter()
                .filter_map(|(slot, prepared)| match prepared {
                    crate::interface::PreparedSlot::Function { callable, export } => {
                        Some(((*link, *slot), (*callable, export.clone())))
                    }
                    _ => None,
                })
        })
        .collect();
    let Opened { heap, roots, saved } = opened;
    let mut closures = foreign_closures(&heap, roots, program.link)?;
    closures.extend(root.iter().flat_map(|root| root.closures.iter().cloned()));
    let loaded = crate::bytecode::compile::Linked {
        others,
        imports,
        closures,
        functions: root.map_or_else(Vec::new, |root| root.functions.clone()),
        helpers: Some(available.unwrap_or(program.helpers())),
    };
    let mut code = crate::bytecode::compile::compile(
        document,
        &registry,
        program.link,
        &loaded,
        root.map(|root| &root.entry),
        available,
        &program.library.core(available),
    )
    .map_err(linked)?;
    crate::bytecode::inline::inline(&mut code);
    let heights = crate::bytecode::validate::validate(&code).map_err(linked)?;
    crate::bytecode::fold::fold(&mut code);
    crate::bytecode::fuse::fuse(&mut code);
    crate::bytecode::trace::trace(&mut code);
    let off = matches!(bounds.charge, Charge::Off);
    let operations = operations(program, &code, modes)?;
    let mut vm = Vm::new(
        Arc::clone(document),
        Arc::new(code),
        &heights,
        heap,
        used,
        bounds.charge_limit,
        off,
        bounds.call_depth,
        super::datum::Crossing {
            program: program.clone(),
            function_origins: Default::default(),
            captions: Vec::new(),
            host: Vec::new(),
            data: lash_kernel_value::data_limits::KernelDataLimits::standard(),
        },
        saved,
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

/// The export `name` names in `document`.
fn exported<'d>(
    document: &'d Document,
    name: &Name,
) -> Result<&'d lash_kernel_doc::FunctionExport, StartError> {
    match document.exports.get(name) {
        Some(lash_kernel_doc::Export::Function(export)) => Ok(export),
        _ => Err(StartError::UnknownEntry {
            entry: name.clone(),
        }),
    }
}

/// A start on `export`, a declared or library function the run enters.
fn entered(export: &lash_kernel_doc::FunctionExport) -> super::callable::Root<'_> {
    super::callable::Root {
        entry: crate::bytecode::compile::Entry {
            function: export.function,
            convention: &export.convention,
            captures: None,
        },
        closures: Vec::new(),
        functions: Vec::new(),
    }
}

/// A start on the callable `function`, whose export is `export` and whose
/// captures bind `names`.
fn called<'a>(
    function: &lash_kernel_doc::FunctionValue,
    export: &'a lash_kernel_doc::FunctionExport,
    names: Option<&'a [Name]>,
) -> super::callable::Root<'a> {
    let (closures, functions) = super::callable::held(function);
    super::callable::Root {
        entry: crate::bytecode::compile::Entry {
            function: export.function,
            convention: &export.convention,
            captures: names,
        },
        closures,
        functions,
    }
}

fn linked(problem: impl ToString) -> StartError {
    StartError::Linked {
        problem: problem.to_string(),
    }
}

#[cfg(test)]
impl CoreMachine {
    /// What the run's heap holds now, apart from what a refusal reports.
    pub(crate) fn held(&self) -> u64 {
        self.vm.heap.held()
    }
}

impl CoreMachine {
    #[cfg(test)]
    pub(super) fn call_law_vm(&mut self) -> &mut Vm<SlotHeap> {
        &mut self.vm
    }

    /// What the tokens of this run's counts and context name.
    #[cfg(any(test, feature = "perf-witness"))]
    pub(crate) fn guest_symbols(&self) -> crate::observe::GuestSymbols {
        crate::observe::GuestSymbols::of(&self.vm.program)
    }

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
        self.vm.tasks.paged = self.vm.tasks.all.len();
        crate::durable::seal(&mut self.vm, self.header.start.clone(), reads)
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

    /// How many waits the run holds issued.
    /// Observes the run's counts, its path, both or neither from here,
    /// keeping its limiters and its charge tape.
    pub(crate) fn watch(&mut self, observing: bool, path: bool) {
        let tape = self.vm.watch.take().and_then(|watch| watch.tape);
        self.vm.watch =
            Watch::new(&self.document, &self.vm.program, observing, path, tape).map(Box::new);
    }

    /// The page port the run's heap seals through.
    #[cfg(test)]
    pub(crate) fn page_access(
        &self,
    ) -> Option<&lash_kernel_state::kernel_internals::tree::Access<'static>> {
        self.vm.heap.access()
    }

    #[cfg(test)]
    pub(crate) fn wait_count(&self) -> u64 {
        self.vm.tasks.waits.len() as u64
    }

    /// Collects at each statement boundary: the laws' collected runs.
    #[cfg(test)]
    pub(crate) fn collect_at_steps(&mut self) {
        self.vm.collecting = true;
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

impl CoreMachine {
    /// [`Machine::start`] under the deployment's data `limits` instead of
    /// the standard ones; a reopened run decodes under the standard ones.
    ///
    /// # Errors
    ///
    /// [`Machine::start`]'s.
    pub fn start_with_data_limits(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        limits: lash_kernel_value::data_limits::KernelDataLimits,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        CellStart::admit_host_metadata(start.host.iter().map(|(name, _)| name.as_str()))?;
        // A program changed since it was prepared is refused before it runs.
        program.validate_links()?;
        let document = program.document.clone();
        let (entry, callable) = match &start.target {
            Target::Main => (None, None),
            Target::Entry(name) => (Some(name.clone()), None),
            Target::Callable(function) => (
                None,
                Some((
                    function,
                    super::callable::export(&program, function, limits)?,
                )),
            ),
        };
        let export = match (&entry, &callable) {
            (Some(name), _) => Some(exported(&document, name)?),
            (None, Some((_, export))) => Some(export),
            (None, None) => None,
        };
        let names = callable
            .as_ref()
            .map(|(function, _)| super::callable::names(function));
        let root = match (&callable, export) {
            (Some((function, export)), _) => Some(called(function, export, names.as_deref())),
            (None, Some(export)) => Some(entered(export)),
            (None, None) => None,
        };
        let params = export.map_or_else(Vec::new, |export| export.signature.params.clone());
        let output = export.and_then(|export| Output::of(program.link, export));
        let (heap, session) = match start.session.as_ref().map(SessionRoot::page) {
            None => (
                SlotHeap::durable(
                    heap_bound(&bounds),
                    crate::heap::open::Durable {
                        directory: lash_kernel_state::paged::run::directory::Directory::new(source),
                        high_water: lash_kernel_state::paged::run::ids::HighWater(0),
                    },
                ),
                None,
            ),
            Some(page) => {
                let (session, directory) =
                    lash_kernel_state::paged::run::session::open(source, page)?;
                let (heap, values) = SlotHeap::open(heap_bound(&bounds), &session.heap, directory)
                    .map_err(|error| StartError::Bindings {
                        problem: format!("the session's heap does not open: {error:?}"),
                    })?;
                (heap, Some((session, values, page)))
            }
        };
        let roots = session
            .as_ref()
            .map_or(&[][..], |(_, values, _)| values.as_slice());
        let opened = Opened {
            heap,
            roots,
            saved: Default::default(),
        };
        let (mut vm, convention) = build(
            &program,
            bounds,
            root.as_ref(),
            start.modes,
            opened,
            0,
            None,
        )?;
        vm.value_depth = limits.value_depth();
        vm.crossing.data = limits;
        let session_names: Vec<Name> = session
            .as_ref()
            .map(|(session, ..)| session.names.clone())
            .unwrap_or_default();
        if let Some((session, values, page)) = session {
            crate::durable::session::bind(&mut vm, &session, values, page);
        }
        let mut header = Header::of(&program, bounds, start.owner);
        header.host = start.host.iter().map(|(name, _)| name.clone()).collect();
        vm.crossing.host.clone_from(&header.host);
        header.start = match (entry, &callable) {
            (Some(name), _) => continuation::RunStart::Entry(name),
            (None, Some((function, _))) => continuation::RunStart::Callable(Box::clone(function)),
            (None, None) => continuation::RunStart::Main,
        };
        let code = Arc::clone(&vm.program);
        let decoder = Decoder {
            max_depth: vm.value_depth,
            policy: document.numbers,
            declared: &|callable| code.callable(callable),
        };
        let mut args = Vec::with_capacity(params.len());
        if let Some(decoded) = crate::bytecode::conventions::arguments(
            &document,
            &code,
            export,
            &start.args,
            vm.value_depth,
        )? {
            for datum in &decoded {
                args.push(Self::copy_in(&mut vm, datum)?);
            }
        } else {
            for (at, param) in params.iter().enumerate() {
                let value = match start.args.get(at) {
                    Some(datum) => {
                        let decoded = decoder.decode(datum, &param.ty).map_err(
                            crate::bytecode::conventions::refused(
                                code.link,
                                export.ok_or_else(|| linked("a start parameter has no export"))?,
                                param,
                            ),
                        )?;
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
        vm.unbound = globals
            .iter()
            .map(|name| {
                !start.host.iter().any(|(bound, _)| bound == name) && !session_names.contains(name)
            })
            .collect();
        let main = vm.program.main;
        let closure = match &callable {
            Some((function, _)) => Some(
                super::callable::restore(&mut vm, function)
                    .map_err(|flow| linked(format!("{flow:?}")))?,
            ),
            None => None,
        };
        vm.start(main, &args, closure)
            .map_err(|flow| linked(format!("{flow:?}")))?;
        Ok(Self {
            vm,
            document,
            ended: false,
            partial: None,
            save: None,
            convention,
            output,
            header,
        })
    }
}

impl Machine for CoreMachine {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        Self::start_with_data_limits(
            program,
            bounds,
            start,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
            source,
        )
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
        let (mut record, directory) =
            continuation::open(source, root.heap).map_err(OpenError::Heap)?;
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
        // A sealed run's constants bind under the image they were placed
        // in; under another, the program places its own (contract §7).
        if changed.is_some() {
            record.constants.clear();
        }
        let main = matches!(record.start, continuation::RunStart::Main);
        if matches!(root.target, CellTargetKind::Main) != main {
            return Err(malformed("the root's target and its run's start disagree"));
        }
        header.host = root.host.clone();
        header.start = record.start.clone();
        let (heap, values) = SlotHeap::open(heap_bound(&bounds), &record.heap, directory)
            .map_err(|error| malformed(format!("the run's heap does not open: {error:?}")))?;
        if record
            .constants
            .windows(2)
            .any(|pair| (pair[0].link, pair[0].key) >= (pair[1].link, pair[1].key))
        {
            return Err(malformed("the run's constants are not ascending"));
        }
        let saved = record
            .constants
            .iter()
            .map(|constant| {
                values
                    .get(constant.value as usize)
                    .map(|value| ((constant.link, constant.key), *value))
                    .ok_or_else(|| {
                        malformed(format!(
                            "constant value {} is past the run's roots",
                            constant.value
                        ))
                    })
            })
            .collect::<Result<_, _>>()?;
        let modes = record
            .reads
            .iter()
            .map(|use_| (*use_, OperationMode::Read))
            .collect();
        // The run's program is compiled again on what it started on.
        let export = match &record.start {
            continuation::RunStart::Main => None,
            continuation::RunStart::Entry(name) => {
                Some(exported(&program.document, name).map_err(|error| {
                    malformed(format!("the run's entry is not exported: {error:?}"))
                })?)
            }
            continuation::RunStart::Callable(_) => None,
        };
        let callable = match &record.start {
            continuation::RunStart::Callable(function) => Some((
                function.as_ref(),
                super::callable::export(
                    &program,
                    function,
                    lash_kernel_value::data_limits::KernelDataLimits::standard(),
                )
                .map_err(|error| {
                    malformed(format!("the run's callable does not start: {error:?}"))
                })?,
            )),
            _ => None,
        };
        let names = callable
            .as_ref()
            .map(|(function, _)| super::callable::names(function));
        let root = match (&callable, export) {
            (Some((function, export)), _) => Some(called(function, export, names.as_deref())),
            (None, Some(export)) => Some(entered(export)),
            (None, None) => None,
        };
        let output = callable
            .as_ref()
            .map(|(_, export)| export)
            .or(export)
            .and_then(|export| Output::of(program.link, export));
        let (mut vm, convention) = build(
            &program,
            bounds,
            root.as_ref(),
            modes,
            Opened {
                heap,
                roots: &values,
                saved,
            },
            record.charged,
            changed.map(|helpers| helpers.as_ref()),
        )
        .map_err(|error| malformed(format!("the document does not compile again: {error:?}")))?;
        vm.crossing.host.clone_from(&header.host);
        crate::durable::admit(&vm.program, &vm.tasks.operations, &record)?;
        crate::durable::open(&mut vm, &record, &values).map_err(malformed)?;
        vm.tasks.paged = vm.tasks.all.len();
        if changed.is_some() {
            crate::durable::live(&vm).map_err(unavailable_helper)?;
        }
        Ok(Self {
            vm,
            document: Arc::clone(&program.document),
            ended: false,
            partial: None,
            save: None,
            convention,
            output,
            header,
        })
    }

    fn set_options(&mut self, options: MachineOptions) {
        self.vm.limiter = options.charge_limiter;
        self.vm.memory_limiter = options.memory_limiter;
        self.watch(!options.observation.is_off(), options.trace_path);
        self.save = options.cell_save;
        self.vm.crossing.function_origins = options.function_origins;
        self.vm.crossing.captions = options.captions;
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
        self.vm.catches.take()
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        self.partial = None;
        match self.vm.run(host, slice) {
            Stop::Returned(result) => {
                let result = crate::bytecode::conventions::result(self.convention.as_ref(), result);
                if let Some(error) = self.unfit_output(&result)? {
                    return self.ended(End::Error(RunError::Uncaught { error, site: None }));
                }
                self.end_cell(None, false, result)
            }
            Stop::Finish(result) => {
                let ender = Some(self.vm.tasks.running);
                self.end_cell(ender, true, result)
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
                // The limits the run's limiters grew, not the ones it
                // started with: the reopened run resumes under them
                // (FIG-6768). An Off run's meter has no limit to grow.
                memory: (header.bounds.memory_meter == crate::MemoryMeter::On)
                    .then(|| self.vm.heap.bound()),
                charge: if self.vm.off() {
                    header.bounds.charge_limit
                } else {
                    self.vm.charge_limit()
                },
            },
            kernel: header.kernel,
            document: header.document,
            link: header.link,
            closure: header.closure,
            helper_library: header.helper_library,
            owner: header.owner,
            target: match header.start {
                continuation::RunStart::Main => CellTargetKind::Main,
                _ => CellTargetKind::Entry,
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
            // A refused run reports the charge its refused event would
            // have made.
            charged: (!self.vm.off()).then(|| self.vm.refused.unwrap_or(self.vm.charged())),
            memory: match self.header.bounds.memory_meter {
                // A refused run reports the total its refused allocation
                // would have held.
                crate::MemoryMeter::On => self.vm.refused_memory.unwrap_or(self.vm.heap.held()),
                crate::MemoryMeter::Off => 0,
            },
            live_tasks: self.vm.tasks.live,
        }
    }
}

/// What the run's heap is bounded by: its memory bound, or nothing under
/// `MemoryMeter::Off`, which keeps no bound to refuse at (FIG-6731).
fn heap_bound(bounds: &Bounds) -> u64 {
    match bounds.memory_meter {
        crate::MemoryMeter::On => bounds.memory,
        crate::MemoryMeter::Off => u64::MAX,
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
        match self.vm.tasks.outstanding(ender, &self.vm.program) {
            Some(error) => self.ended(End::Error(error)),
            None => self.ended(end(result)),
        }
    }

    /// The run finished with `result`, ended by `ender`: a `main` cell
    /// hands its session on (`K-SES-001`), an entry or a callable nothing.
    fn end_cell(
        &mut self,
        ender: Option<usize>,
        finish: bool,
        result: Datum,
    ) -> Result<Step, MachineError> {
        if let Some(error) = self.vm.tasks.outstanding(ender, &self.vm.program) {
            return self.ended(End::Error(error));
        }
        let (carry, not_carried) = match self.header.start {
            continuation::RunStart::Entry(_) | continuation::RunStart::Callable(_) => {
                (None, Vec::new())
            }
            continuation::RunStart::Main => {
                self.ended = true;
                let mut save = self.save.take();
                let (carry, not_carried) = crate::durable::session::carry(
                    &mut self.vm,
                    &self.header.host,
                    save.as_deref_mut(),
                )?;
                self.save = save;
                (Some(carry), not_carried)
            }
        };
        self.ended(End::Finished(Box::new(Finished {
            result,
            finish,
            carry,
            not_carried,
        })))
    }

    /// The entry's typed mismatch `result` makes as it leaves the run,
    /// unless it fits the start's public result.
    fn unfit_output(&self, result: &Datum) -> Result<Option<Datum>, MachineError> {
        let Some(output) = &self.output else {
            return Ok(None);
        };
        let code = &self.vm.program;
        let decoder = Decoder {
            max_depth: self.vm.value_depth,
            policy: self.document.numbers,
            declared: &|callable| code.callable(callable),
        };
        let Err(lash_kernel_doc::DecodeRefusal::Type { mismatch }) =
            decoder.decode(result, &output.result)
        else {
            return Ok(None);
        };
        let kind = "entry_type_mismatch";
        let data = lash_kernel_doc::diagnostic_datum(&lash_kernel_value::EntryTypeMismatch {
            callable: output.callable,
            mismatch: mismatch.within("output"),
        })
        .map_err(|error| MachineError::Fault {
            problem: error.to_string(),
        })?;
        Ok(Some(Datum::Error(Box::new(
            lash_kernel_doc::ErrorDatum::new(kind.into(), kind.into(), data),
        ))))
    }

    fn uncaught(&mut self, value: Value) -> Result<Step, MachineError> {
        let site = self.vm.raised_at.take();
        let error = self.vm.copy_stopped(value);
        match error {
            Ok(error) => self.ended(End::Error(RunError::Uncaught { error, site })),
            Err(super::datum::StoppedCopy::Raised(raised)) => {
                let error = Datum::Error(Box::new(lash_kernel_doc::ErrorDatum::new(
                    raised.kind.into(),
                    raised.message,
                    Datum::Null,
                )));
                self.ended(End::Error(RunError::Uncaught { error, site }))
            }
            Err(super::datum::StoppedCopy::Fault(error)) => {
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
    // A perform is filled in the Link whose code runs it: an own unit's
    // Link, and for library code every Link that may call it.
    let mut performs = Vec::new();
    for unit in &code.units {
        for action in &unit.actions {
            if let ActionSite::Perform { owner, slot, .. } = action {
                performs.push((unit.link, *owner, *slot));
            }
        }
    }
    for &(owner, slot) in &code.lib.performs {
        for &link in &code.links {
            performs.push((link, owner, slot));
        }
    }
    let mut operations = HashMap::new();
    for (link, owner, slot) in performs {
        let (owner, slot) = (&owner, &slot);
        operations.entry((link, *owner, *slot)).or_insert_with(|| {
            let instance = program
                .instances
                .get(&link)
                .ok_or("the perform's Link is not loaded")?;
            let document = program
                .documents
                .get(&instance.document)
                .ok_or("the perform's document is not loaded")?;
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
    Ok(operations)
}
