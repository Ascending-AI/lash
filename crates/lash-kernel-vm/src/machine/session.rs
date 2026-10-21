//! Starting and carrying a cell preserve the paged identity directory.
use super::*;
use crate::compile::BindingName;
use crate::interface::{CellStart, Finished, Target};
use lash_kernel_doc::NewObject;
use lash_kernel_state::paged::{AllocationCensus, CarryRoots, EditEpoch, KeyedRoot, PageSource};

pub(super) fn empty_root() -> KeyedRoot {
    KeyedRoot::Empty
}
pub(super) fn open_tree<T: lash_kernel_state::kernel_internals::tree::Cell>(
    root: KeyedRoot,
) -> Tree<T> {
    Tree::of_keyed(&root)
}
pub(crate) fn start(
    program: Program,
    bounds: Bounds,
    start: CellStart,
    source: Box<dyn PageSource>,
    census: Option<Box<dyn AllocationCensus>>,
    layout: Layout,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<KernelMachine, StartError> {
    CellStart::admit_host_metadata(start.host.iter().map(|(name, _)| name.as_str()))?;
    program.validate_links()?;
    let library = program.library.in_charge(bounds.charge).in_layout(layout);
    let admitted = library.admit(&program.document, limits)?;
    let exe = admitted
        .executable(
            &program.document,
            &library,
            &program.documents,
            &program.instances,
        )
        .map_err(|missing| StartError::MissingFunction {
            function: missing.0,
        })?;
    let mut heap = match &start.session {
        Some(session) => PagedHeap::open_header(
            lash_kernel_state::kernel_internals::session_heap(session),
            source,
            EditEpoch(0),
        )?,
        None => PagedHeap::fresh(source, EditEpoch(0)),
    };
    heap.set_data_limits(limits);
    if bounds.memory_meter != crate::MemoryMeter::Off {
        heap.set_census(
            census.ok_or_else(|| HeapFault::Malformed("metered start has no census".into()))?,
        )?;
    }
    heap.set_family_base(Box::new(
        lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let functions = start.session.as_ref().map_or_else(empty_root, |session| {
        lash_kernel_state::kernel_internals::session_functions(session)
    });
    if let Some(session) = &start.session {
        heap.adopt(session)?;
        if bounds.memory_meter != crate::MemoryMeter::Off
            && heap.certificate().metric == lash_kernel_state::paged::AccountingMetric::Unmetered
        {
            heap.collect()?;
        }
    }
    if bounds.memory_meter == crate::MemoryMeter::Off {
        heap.without_memory_accounting();
    }
    let access = heap.pages().clone();
    let arena: SharedArena = std::rc::Rc::new(std::cell::RefCell::new(
        lash_kernel_state::arena::Arena::from_ordinal(heap.next_ordinal()),
    ));
    let mut machine = KernelMachine {
        active_link: program.link,
        closure: program.closure,
        owner: start.owner,
        census_refinements: heap.certificate().refinements,
        costs: crate::costs::Costs::of_document(&program.document),
        program,
        document: None,
        exe: Arc::clone(&exe),
        bounds,
        capacity_floor: 0,
        heap,
        arena: std::rc::Rc::clone(&arena),
        limits,
        tasks: tasks::Tasks::new(access.clone(), std::rc::Rc::clone(&arena), Arc::clone(&exe)),
        ready: queue::Queue::open(Tree::new(true), access.clone()),
        current: Some(TaskId::MAIN),
        joins: Tree::new(false),
        next_join: 0,
        waits: Tree::new(false),
        next_wait: 0,
        issued: None,
        withdrawn: queue::Queue::open(Tree::new(true), access.clone()),
        withdrawn_ever: Tree::new(false),
        modes: Tree::new(false),
        functions,
        host: Vec::new(),
        save: None,
        captions: Vec::new(),
        limiters: Limiters::default(),
        session_cell: matches!(start.target, Target::Main),
        session: Default::default(),
        pins: Pins::default(),
        projecting: false,
        charging: false,
        paced: false,
        region: None,
        slice_end: u64::MAX,
        sites: Default::default(),
        unproductive: Vec::new(),
        islands: Default::default(),
        plans: 0,
        segments: Default::default(),
        charged: 0,
        live_tasks: 1,
        regs: registers::Registers::default(),
        storage: Storage::default(),
        steps: Default::default(),
        end: None,
        ended: false,
        observer: None,
        ran: Vec::new(),
        raised: None,
        catches: Default::default(),
        #[cfg(test)]
        boundary: None,
        #[cfg(test)]
        boundaries: 0,
    };
    machine.capacity_counted();
    machine.bind_modes(start.modes)?;
    machine.tasks.push(Task::fresh(
        lash_kernel_state::paged::TaskIdentityRef::MAIN,
        None,
        access,
        arena,
        Arc::clone(&exe),
    ))?;
    let arguments = |problem: String| StartError::Arguments { problem };
    // Host-supplied start values are not guest work (`K-CHG-002`).
    // The first statement selects charging from its prepared code.
    let descriptor = match &start.target {
        Target::Main => None,
        Target::Entry(name) => match machine.program.document.exports.get(name) {
            Some(lash_kernel_doc::Export::Function(export)) => Some(export.clone()),
            _ => {
                return Err(StartError::UnknownEntry {
                    entry: name.clone(),
                });
            }
        },
        Target::Callable(function) => {
            if function.callable.link != machine.program.link {
                return Err(arguments(
                    "callable Link differs from the program root".into(),
                ));
            }
            let definition = machine
                .program
                .document
                .functions
                .get(&function.callable.function)
                .ok_or_else(|| {
                    arguments("callable definition is absent from the defining Link".into())
                })?;
            lash_kernel_doc::validate_callable_signature(
                &function.signature,
                &function.convention,
                &definition.signature,
            )
            .map_err(StartError::NotAdmitted)?;
            for target in match &function.convention {
                lash_kernel_doc::StartConvention::Direct => Vec::new(),
                lash_kernel_doc::StartConvention::TsDirect { await_fn } => vec![await_fn],
                lash_kernel_doc::StartConvention::TsArgsList { await_fn, same_fn } => {
                    vec![await_fn, same_fn]
                }
                lash_kernel_doc::StartConvention::PyDirect { cleanup } => vec![cleanup],
            } {
                let declared = match target {
                    lash_kernel_doc::FunctionTarget::Local(id) => {
                        machine.program.document.functions.contains_key(id)
                            || machine.program.document.requires.natives.contains(id)
                    }
                    lash_kernel_doc::FunctionTarget::Import(slot) => machine
                        .program
                        .document
                        .imports
                        .get(slot)
                        .is_some_and(|import| {
                            matches!(import.kind, lash_vocabulary::ImportKind::Code { .. })
                        }),
                };
                if !declared {
                    return Err(arguments(
                        "callable convention helper is absent from its defining Link".into(),
                    ));
                }
            }
            function
                .check(
                    machine.program.library.registry().as_ref(),
                    lash_kernel_doc::FunctionValueLimits::standard(),
                    limits,
                )
                .map_err(|error| arguments(error.to_string()))?;
            Some(lash_kernel_doc::FunctionExport {
                function: function.callable.function,
                signature: function.signature.clone(),
                convention: function.convention.clone(),
                asynchronous: false,
                rest: false,
            })
        }
    };
    let (code, args) = match &start.target {
        Target::Main => {
            if !start.args.is_empty() {
                return Err(arguments("`main` takes no arguments".into()));
            }
            // The host's bindings are `main`'s over the session's, and
            // charge nothing: the cell did not make them.
            for (name, datum) in &start.host {
                let value = machine
                    .decode(&exe, datum, &lash_vocabulary::types::Type::Any)
                    .map_err(|error| start_interrupt(name.as_str(), error))?;
                machine
                    .session_write(&BindingName::new(name.clone()), value, false)
                    .map_err(|error| start_interrupt(name.as_str(), error.into()))?;
                machine.host.push(name.clone());
            }
            (
                Some(
                    exe.main
                        .ok_or_else(|| arguments("the document has no Main".into()))?,
                ),
                Vec::new(),
            )
        }
        Target::Entry(_) | Target::Callable(_) => {
            if start.session.is_some() {
                return Err(StartError::Bindings {
                    problem: "an entry starts without a session".into(),
                });
            }
            let definition = descriptor.as_ref().unwrap();
            let entry = "callable";
            let code = exe.declared.get(&definition.function).copied().or_else(|| {
                exe.code_at(&lash_vocabulary::sites::Site::new(
                    lash_vocabulary::sites::Unit::Library(definition.function),
                    [],
                ))
            });
            if code.is_none() && exe.lib_of(&definition.function).is_none() {
                return Err(arguments("start function is not loaded".into()));
            }
            let signature = &definition.signature;
            let given = start.args.len();
            if matches!(
                definition.convention,
                lash_kernel_doc::StartConvention::Direct
                    | lash_kernel_doc::StartConvention::PyDirect { .. }
            ) && !(signature.required()..=signature.params.len()).contains(&given)
            {
                return Err(arguments(format!(
                    "`{entry}` takes {} to {} argument(s); {given} given",
                    signature.required(),
                    signature.params.len()
                )));
            }
            let mut args = Vec::new();
            for (datum, param) in start.args.iter().zip(&signature.params) {
                machine.regs = args.clone().into();
                args.push(if param.optional && matches!(datum, Datum::Absent) {
                    Value::Absent
                } else {
                    machine
                        .decode(&exe, datum, &param.ty)
                        .map_err(|error| start_interrupt(param.name.as_str(), error))?
                });
            }
            if let lash_kernel_doc::StartConvention::TsArgsList { .. } = definition.convention {
                for datum in start.args.iter().skip(signature.params.len()) {
                    machine.regs = args.clone().into();
                    args.push(
                        machine
                            .decode(&exe, datum, &lash_vocabulary::types::Type::Any)
                            .map_err(|error| start_interrupt("extra argument", error))?,
                    );
                }
                // Start arguments remain roots while the packed list reserves.
                machine.regs = args.clone().into();
                let list = machine
                    .new_object(NewObject::List(args))
                    .map_err(|error| start_interrupt("arguments", error.into()))?;
                args = vec![Value::Absent, list];
            }
            if matches!(
                definition.convention,
                lash_kernel_doc::StartConvention::TsDirect { .. }
            ) {
                let arity = code
                    .map(|code| exe.code(code).params.len())
                    .or_else(|| {
                        exe.lib_of(&definition.function)
                            .map(|lib| exe.lib(lib).arity)
                    })
                    .ok_or_else(|| arguments("start function is absent".into()))?;
                args.truncate(arity);
            }
            (code, args)
        }
    };
    let count = args.len();
    machine.regs = args.into();
    if let Some(code) = code {
        let mut call = exec::Call::new(code, 0, count);
        if let Some(descriptor) = &descriptor
            && !exe.code(code).charging
            && let Some(lib) = exe.lib_of(&descriptor.function)
        {
            call = call.of_library(lib);
        }
        machine
            .push_frame(TaskId::MAIN, &exe, call)
            .map_err(|error| start_interrupt("the start", error))?;
    } else {
        let descriptor = descriptor
            .as_ref()
            .ok_or_else(|| arguments("start descriptor is absent".into()))?;
        let lib = exe
            .lib_of(&descriptor.function)
            .ok_or_else(|| arguments("start native is absent".into()))?;
        machine
            .refresh_charging(TaskId::MAIN)
            .map_err(|e| start_interrupt("start charge", e.into()))?;
        match machine.call_lib_linked(
            TaskId::MAIN,
            &exe,
            lib,
            machine.program.link,
            0,
            count,
            None,
        ) {
            Ok(super::ops::Flow::Done) => {
                let value = machine.take(0);
                let result =
                    match machine.check_public_result(&exe, &value, &descriptor.signature.result) {
                        Ok(()) => Ok(value),
                        Err(error) => Err(*machine
                            .thrown_by(TaskId::MAIN, error)
                            .map_err(|e| start_interrupt("start result", e.into()))?),
                    };
                machine
                    .end_task(TaskId::MAIN, result)
                    .map_err(|e| start_interrupt("start result", e.into()))?;
            }
            Ok(super::ops::Flow::Step) => {}
            Err(error) => {
                let value = machine
                    .thrown_by(TaskId::MAIN, error)
                    .map_err(|e| start_interrupt("start native", e.into()))?;
                machine
                    .end_task(TaskId::MAIN, Err(*value))
                    .map_err(|e| start_interrupt("start native", e.into()))?;
            }
        }
    }
    if let Some(descriptor) = &descriptor
        && machine.end.is_none()
    {
        machine
            .frame(TaskId::MAIN)
            .map_err(|e| start_interrupt("start frame", e.into()))?
            .convention = Some(descriptor.convention.clone());
        machine
            .frame(TaskId::MAIN)
            .map_err(|e| start_interrupt("start result", e.into()))?
            .public_result = Some(descriptor.signature.result.clone());
    }
    if let Target::Callable(function) = &start.target
        && let Some(code) = code
    {
        if exe.code(code).free.len() != function.captures.len()
            || exe
                .code(code)
                .free
                .iter()
                .any(|(name, _)| !function.captures.contains_key(name))
        {
            return Err(arguments(
                "callable captures differ from its lexical environment".into(),
            ));
        }
        for (name, local) in &exe.code(code).free {
            if let Some(datum) = function.captures.get(name) {
                let value = machine
                    .decode(&exe, datum, &lash_vocabulary::types::Type::Any)
                    .map_err(|e| start_interrupt(name.as_str(), e))?;
                machine
                    .bind(TaskId::MAIN, local.at, true, value)
                    .map_err(|e| start_interrupt(name.as_str(), e.into()))?;
            }
        }
    }
    machine.regs.clear();
    machine
        .publish_roots()
        .map_err(|error| start_interrupt("the start", error.into()))?;
    machine.heap.release_statement_roots();
    Ok(machine)
}
/// The session bindings a cell read or wrote, by binding key: arena values
/// over the records the heap's slots hold, given their page forms only at
/// a projection point (seam C, FIG-6748).
#[derive(Default)]
pub(super) struct Session {
    bindings: std::collections::HashMap<Arc<[u8]>, Resident>,
    /// How many writes the cell has made: what orders the bindings it
    /// wrote since the last projection point.
    writes: u64,
}

impl Session {
    /// Visits the value of every binding the cell holds: what an arena
    /// collection keeps.
    pub(super) fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        self.bindings.values().for_each(|bound| visit(&bound.value));
    }

    /// The resident value of the binding under `key`: what a region holds
    /// as its session slot from its entry (`docs/design/kernel-regions.md`
    /// §3.6).
    pub(super) fn resident(&self, key: &Arc<[u8]>) -> Option<Value> {
        self.bindings.get(key).map(|bound| bound.value)
    }

    /// A region's write of the resident binding under `key`, published at
    /// its exit: the binding holds `value`, written now, as the slot write
    /// that wrote it would have left it. Its holding was admitted when the
    /// region wrote it.
    pub(super) fn publish(&mut self, key: &Arc<[u8]>, value: Value) -> bool {
        let Some(bound) = self.bindings.get_mut(key) else {
            return false;
        };
        bound.value = value;
        bound.written = Some(self.writes);
        self.writes += 1;
        true
    }

    /// The bindings written since the last projection point, in the order
    /// their values were bound: the order write-time ids took, so where a
    /// projection point falls moves no id among them.
    fn written(&self) -> Vec<(&Arc<[u8]>, &Resident)> {
        let mut written: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, bound)| bound.written.is_some())
            .collect();
        written.sort_unstable_by_key(|(_, bound)| bound.written);
        written
    }

    /// The keys of the bindings written since the last projection point,
    /// giving `out` what their values reach (seam I, FIG-6748).
    pub(super) fn reach(
        &self,
        arena: &mut lash_kernel_state::arena::Arena,
        reach: &mut lash_kernel_state::arena::Reach,
        out: &mut dyn FnMut(lash_kernel_state::arena::Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<Vec<Vec<u8>>, HeapFault> {
        let mut written = Vec::new();
        for (key, bound) in self.written() {
            written.push(key.to_vec());
            arena.reach(reach, &bound.value, out)?;
        }
        Ok(written)
    }
}

/// A session binding the cell read or wrote.
struct Resident {
    name: BindingName,
    value: Value,
    /// The write that bound the value, when the heap's slot does not hold
    /// it yet.
    written: Option<u64>,
}

/// Why a binding that reaches a closure or a task is not carried, when no
/// dialect says more (`K-SES-003`).
const NOT_CARRIED: &str = "it holds a function or a task, which ends with its cell";

impl KernelMachine {
    /// Saves the functions among `dropped` through the cell's saving, if it
    /// has one (`K-SES-004`): each saved function's record goes into the
    /// session's functions, and each binding that held one is bound to its
    /// value in `carried`. Answers the carried bindings, the functions and
    /// every binding left behind, with why.
    #[expect(clippy::type_complexity, reason = "the carry's three parts")]
    fn save_dropped(
        &mut self,
        carried: KeyedRoot,
        dropped: &[Name],
    ) -> Result<(KeyedRoot, KeyedRoot, Vec<(Name, String)>), Halt> {
        let unsaved = || {
            dropped
                .iter()
                .map(|name| (name.clone(), NOT_CARRIED.to_owned()))
                .collect::<Vec<_>>()
        };
        if dropped.is_empty() {
            return Ok((carried, self.functions, Vec::new()));
        }
        let Some(mut save) = self.save.take() else {
            return Ok((carried, self.functions, unsaved()));
        };
        let functions: Tree<lash_kernel_state::paged::FunctionRecord> = open_tree(self.functions);
        let mut names = Vec::new();
        functions.visit(self.heap.pages(), &mut |_, record| {
            names.push(record.name.clone());
            true
        })?;
        let held = names
            .iter()
            .map(|name| name.resolve(self.heap.pages()))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let mut bindings = std::collections::BTreeMap::new();
        for name in dropped.iter().chain(&held) {
            if let Some(value) = self.heap.session_binding(name)? {
                bindings.insert(name.clone(), value);
            }
        }
        let document = Arc::clone(&self.program.document);
        let closure = lash_kernel_doc::heap::PageRef {
            id: self.closure,
            grant: lash_kernel_doc::heap::PageGrant::NONE,
        };
        let documents = self
            .program
            .instances
            .iter()
            .filter_map(|(link, instance)| {
                self.program
                    .documents
                    .get(&instance.document)
                    .map(|doc| (*link, doc.as_ref().clone()))
            })
            .collect();
        let saved = save.save(
            &mut crate::CellSaveContext::new(&mut self.heap),
            crate::paged::SaveCell {
                link: self.program.link,
                documents: &documents,
                document: &document,
                closure,
                bindings: &bindings,
                dropped,
                held: &held,
                catalog: self.program.library.registry().as_ref(),
            },
        );
        self.save = Some(save);
        let saved = saved?;
        let mut functions = functions;
        for (name, record) in saved.functions {
            functions.write(
                lash_kernel_state::kernel_internals::metadata::metadata_key(&name),
                record,
                self.heap.pages(),
            )?;
        }
        let mut bindings: Tree<BindingRecord> = open_tree(carried);
        let mut kept = std::collections::BTreeSet::new();
        for (name, value) in saved.rebound {
            let record = self.binding_record(&name, value)?;
            bindings.write(
                lash_kernel_state::kernel_internals::metadata::metadata_key(&name),
                record,
                self.heap.pages(),
            )?;
            kept.insert(name);
        }
        let mut refused: std::collections::BTreeMap<Name, String> =
            saved.refused.into_iter().collect();
        let not_carried = dropped
            .iter()
            .filter(|name| !kept.contains(*name))
            .map(|name| {
                let why = refused
                    .remove(name)
                    .unwrap_or_else(|| NOT_CARRIED.to_owned());
                (name.clone(), why)
            })
            .collect();
        let mut work = lash_kernel_state::kernel_internals::tree::SealWork::new(
            lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            },
        );
        functions.seal(self.heap.pages(), &mut work)?;
        bindings.seal(self.heap.pages(), &mut work)?;
        Ok((keyed(&bindings)?, keyed(&functions)?, not_carried))
    }

    /// The record of the session binding `name` holding `value`, declared at
    /// `main`.
    fn binding_record(&mut self, name: &Name, value: PageValue) -> Result<BindingRecord, Halt> {
        binding_record(
            &mut self.heap,
            &self
                .exe
                .code(
                    self.exe
                        .main
                        .ok_or_else(|| super::fault("a session cell has no Main"))?,
                )
                .site,
            name,
            value,
        )
        .map_err(super::write_halt)
    }

    pub(super) fn session_get(&mut self, name: &BindingName) -> Result<Option<Value>, Halt> {
        if let Some(bound) = self.session.bindings.get(&name.key) {
            return Ok(Some(bound.value));
        }
        let bound = self
            .heap
            .load_session_bound_at(&name.key)
            .map_err(super::write_halt)?;
        Ok(match bound {
            None => None,
            Some(BoundRecord::Value(page)) => {
                // The value is what the heap's slot holds: it stays with
                // the cell, clean, until the cell writes the name.
                let value = self.admit_page(&page)?;
                self.session.bindings.insert(
                    Arc::clone(&name.key),
                    Resident {
                        name: name.clone(),
                        value,
                        written: None,
                    },
                );
                Some(value)
            }
            Some(BoundRecord::Shared(id)) => {
                let cell = project::admit_cell(&mut self.arena.borrow_mut(), id);
                Some(self.in_arena(|arena| arena.cell(cell)).map_err(
                    |interrupt| match interrupt {
                        Interrupt::Halt(halt) => *halt,
                        other => fault(&format!("{other:?}")),
                    },
                )?)
            }
        })
    }

    /// Binds `name` to `value`, the arena value, until the next projection
    /// point gives it its page form (seam C): a declaration, or an
    /// assignment when `existing`, which is refused for a name neither the
    /// cell nor its session binds.
    pub(super) fn session_write(
        &mut self,
        name: &BindingName,
        value: Value,
        existing: bool,
    ) -> Result<bool, Halt> {
        // A first assignment of a name reads its slot: whether the session
        // binds it at all.
        if existing
            && !self.session.bindings.contains_key(&name.key)
            && self
                .heap
                .load_session_bound_at(&name.key)
                .map_err(super::write_halt)?
                .is_none()
        {
            return Ok(false);
        }
        let written = Some(self.session.writes);
        self.session.writes += 1;
        match self.session.bindings.get_mut(&name.key) {
            Some(bound) => {
                bound.value = value;
                bound.written = written;
            }
            None => {
                self.session.bindings.insert(
                    Arc::clone(&name.key),
                    Resident {
                        name: name.clone(),
                        value,
                        written,
                    },
                );
            }
        }
        Ok(true)
    }

    /// The handles of the session bindings written since the last
    /// projection point, in the order their values were bound: where the
    /// canonical walk starts (seam C).
    pub(super) fn session_roots(&self, out: &mut Vec<ArenaHandle>) {
        out.extend(
            self.session
                .written()
                .into_iter()
                .filter_map(|(_, bound)| bound.value.handle()),
        );
    }

    /// Gives the new objects the session bindings written since the last
    /// projection point reach their ids, in the canonical walk's order:
    /// what the run does first wherever it saves a value before its
    /// next projection point (a task's result, an effect's argument), as
    /// the walk there would, so where those points fall moves no id.
    pub(super) fn identify_session(&mut self) -> Result<(), Halt> {
        let mut roots = Vec::new();
        self.session_roots(&mut roots);
        if roots.is_empty() {
            return Ok(());
        }
        self.arena
            .borrow_mut()
            .walk(&roots)
            .map_err(|error| fault(&error.to_string()))
    }

    /// Gives the session bindings written since the last projection point
    /// their page forms in the heap's slots.
    pub(super) fn project_session(&mut self) -> Result<(), Halt> {
        let written: Vec<(Arc<[u8]>, Name, Value)> = self
            .session
            .written()
            .into_iter()
            .map(|(key, bound)| (Arc::clone(key), bound.name.name.clone(), bound.value))
            .collect();
        for (key, name, value) in written {
            let page = self.page_form(&value)?;
            let record = self.binding_record(&name, page)?;
            self.heap
                .project_session_binding(&key, record)
                .map_err(super::write_halt)?;
            if let Some(bound) = self.session.bindings.get_mut(&key) {
                bound.written = None;
            }
        }
        Ok(())
    }
    pub(super) fn finished(&mut self, result: Datum, finish: bool) -> Result<Finished, Halt> {
        if !self.session_cell {
            return Ok(Finished {
                result,
                finish,
                carry: None,
                not_carried: Vec::new(),
            });
        }
        self.publish_roots()?;
        // The host's bindings are the host's: what the cell made of its
        // copy is not the session's.
        if !self.host.is_empty() {
            self.heap.seal_session_root(
                &mut lash_kernel_state::kernel_internals::tree::SealWork::new(
                    lash_kernel_state::paged::SealBudget {
                        pages: u64::MAX,
                        bytes: u64::MAX,
                    },
                ),
            )?;
            let mut bindings = self.heap.session_root().clone();
            for name in std::mem::take(&mut self.host) {
                bindings.remove(
                    &lash_kernel_state::kernel_internals::metadata::metadata_key(&name),
                    self.heap.pages(),
                )?;
            }
            self.heap
                .set_session_root(bindings)
                .map_err(super::write_halt)?;
        }
        // Lifecycle roots name sealed trees. A cell may finish before its
        // first park, so its session still has only transient pages here.
        self.heap.seal_session_root(
            &mut lash_kernel_state::kernel_internals::tree::SealWork::new(
                lash_kernel_state::paged::SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                },
            ),
        )?;
        let bindings = keyed(self.heap.session_root())?;
        let classification = self.heap.classify_carry(&bindings)?;
        let mut dropped = classification.dropped;
        // Metadata tree keys are bounded digests; public diagnostics retain
        // their semantic name order independently of those storage keys.
        dropped.sort();
        let (carried, functions, not_carried) =
            self.save_dropped(classification.carried, &dropped)?;
        let carry = self.heap.carry(CarryRoots {
            bindings: carried,
            functions,
        })?;
        Ok(Finished {
            result,
            finish,
            carry: Some(carry),
            not_carried,
        })
    }
}
pub(super) fn keyed<T: lash_kernel_state::kernel_internals::tree::Cell>(
    tree: &Tree<T>,
) -> Result<KeyedRoot, lash_kernel_doc::heap::HeapFault> {
    tree.keyed_root()
}

impl KernelMachine {
    pub(crate) fn start_with_layout(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        layout: Layout,
        mut source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        let census = if bounds.memory_meter == crate::MemoryMeter::Off {
            None
        } else {
            Some(start_census(&mut *source, &start)?)
        };
        self::start(
            program,
            bounds,
            start,
            source,
            census,
            layout,
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )
    }

    /// [`Machine::start`] under the deployment's data limits instead of
    /// the standard ones.
    ///
    /// # Errors
    ///
    /// [`Machine::start`]'s.
    pub fn start_with_data_limits(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        limits: lash_vocabulary::data_limits::KernelDataLimits,
        mut source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        let census = if bounds.memory_meter == crate::MemoryMeter::Off {
            None
        } else {
            Some(start_census(&mut *source, &start)?)
        };
        self::start(
            program,
            bounds,
            start,
            source,
            census,
            Layout::default(),
            limits,
        )
    }
}

#[cfg(test)]
impl KernelMachine {
    /// Starts a run held to `census` instead of the one its session's
    /// accounting restores: the laws that count or limit the census.
    pub(crate) fn start_with_census(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        layout: Layout,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
    ) -> Result<Self, StartError> {
        self::start(
            program,
            bounds,
            start,
            source,
            Some(census),
            layout,
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )
    }
}

/// Why a run's saved memory accounting could not be restored.
pub(crate) enum AccountingFault {
    Heap(lash_kernel_doc::heap::HeapFault),
    Malformed(String),
}

impl From<AccountingFault> for StartError {
    fn from(fault: AccountingFault) -> Self {
        match fault {
            AccountingFault::Heap(fault) => Self::Heap(fault),
            AccountingFault::Malformed(problem) => Self::Accounting { problem },
        }
    }
}

impl From<AccountingFault> for crate::paged::OpenError {
    fn from(fault: AccountingFault) -> Self {
        match fault {
            AccountingFault::Heap(fault) => Self::Heap(fault),
            AccountingFault::Malformed(problem) => Self::Accounting { problem },
        }
    }
}

/// The census a run holds: over the heap whose header is `heap`, a fresh
/// heap's when none. Reads the header page through `source`.
pub(crate) fn restored_census(
    source: &mut dyn PageSource,
    heap: Option<lash_kernel_doc::heap::PageRef>,
) -> Result<Box<dyn AllocationCensus>, AccountingFault> {
    use lash_kernel_state::paged::{AccountingCertificate, CensusFault, heap_accounting};
    let accounting = match heap {
        Some(heap) => heap_accounting(source, heap).map_err(AccountingFault::Heap)?,
        None => AccountingCertificate::FRESH,
    };
    lash_kernel_state::kernel_internals::census_impl::Census::restore(accounting)
        .map(|census| Box::new(census) as Box<dyn AllocationCensus>)
        .map_err(|fault| match fault {
            CensusFault::Heap(fault) => AccountingFault::Heap(fault),
            CensusFault::Malformed(problem) => AccountingFault::Malformed(problem),
        })
}

/// A starting run's census: over its session's heap, or a fresh one.
pub(crate) fn start_census(
    source: &mut dyn PageSource,
    start: &CellStart,
) -> Result<Box<dyn AllocationCensus>, AccountingFault> {
    restored_census(
        source,
        start
            .session
            .as_ref()
            .map(lash_kernel_state::kernel_internals::session_heap),
    )
}

fn start_interrupt(what: &str, error: Interrupt) -> StartError {
    match error {
        Interrupt::Halt(error) => match *error {
            Halt::Heap(error) => StartError::Heap(*error),
            other => StartError::Arguments {
                problem: format!("{what}: {:?}", Interrupt::from(other)),
            },
        },
        other => StartError::Arguments {
            problem: format!("{what}: {other:?}"),
        },
    }
}

fn binding_record(
    heap: &mut PagedHeap,
    declared: &lash_vocabulary::sites::Site,
    name: &Name,
    value: PageValue,
) -> Result<BindingRecord, HeapWriteError> {
    Ok(BindingRecord {
        name: lash_kernel_state::paged::NameRef::new(name, heap.pages())?,
        declared: lash_kernel_state::paged::SiteRef::new(declared, heap.pages())?,
        value: BoundRecord::Value(value),
    })
}
