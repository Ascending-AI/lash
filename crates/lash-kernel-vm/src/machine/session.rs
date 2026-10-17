//! Starting and carrying a cell preserve the paged identity directory.
use super::*;
use crate::compile::BindingName;
use crate::interface::{CellStart, Finished, Target};
use lash_kernel_state::paged::{
    AllocationCensus, CarryRoots, EditEpoch, KeyedRoot, ObjectInit, PageSource,
};

pub(super) fn empty_root() -> KeyedRoot {
    KeyedRoot::Empty
}
pub(super) fn open_tree<T: lash_kernel_state::paged::tree::Cell>(root: KeyedRoot) -> Tree<T> {
    Tree::of_keyed(&root)
}
pub(super) fn start(
    program: Program,
    bounds: Bounds,
    start: CellStart,
    source: Box<dyn PageSource>,
    census: Box<dyn AllocationCensus>,
    layout: Layout,
    limits: lash_kernel_doc::KernelDataLimits,
) -> Result<KernelMachine, StartError> {
    CellStart::admit_host_metadata(start.host.iter().map(|(name, _)| name.as_str()))?;
    let library = program.library.in_layout(layout);
    let admitted = library.admit(&program.document, limits)?;
    let exe = admitted
        .executable(&program.document, &library)
        .map_err(|missing| StartError::MissingFunction {
            function: missing.0,
        })?;
    let mut heap = match &start.session {
        Some(session) => PagedHeap::open_header(session.heap, source, EditEpoch(0))?,
        None => PagedHeap::fresh(source, EditEpoch(0)),
    };
    heap.set_data_limits(limits);
    heap.set_census(census)?;
    heap.set_family_base(Box::new(
        lash_kernel_state::paged::family_impl::FrozenBase::default(),
    ));
    let functions = start
        .session
        .as_ref()
        .map_or_else(empty_root, |session| session.functions);
    if let Some(session) = &start.session {
        heap.adopt(session)?;
    }
    let access = heap.pages().clone();
    let mut machine = KernelMachine {
        release: program.release,
        owner: start.owner,
        census_refinements: heap.certificate().refinements,
        costs: crate::costs::Costs::of_document(&program.document),
        program,
        document: None,
        exe: Arc::clone(&exe),
        bounds,
        heap,
        limits,
        tasks: tasks::Tasks::new(access.clone(), Arc::clone(&exe)),
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
        session_cell: matches!(start.target, Target::Main),
        pins: Pins::default(),
        fresh: Vec::new(),
        charging: true,
        scalar_blocks: false,
        charged: 0,
        live_tasks: 1,
        regs: registers::Registers::default(),
        storage: Storage::default(),
        steps: Default::default(),
        end: None,
        ended: false,
        observer: None,
        ran: Vec::new(),
        #[cfg(test)]
        boundary: None,
        #[cfg(test)]
        boundaries: 0,
    };
    machine.bind_modes(start.modes)?;
    machine.tasks.push(Task::fresh(
        lash_kernel_state::paged::TaskIdentityRef::MAIN,
        access,
        Arc::clone(&exe),
    ))?;
    let arguments = |problem: String| StartError::Arguments { problem };
    // What the start decodes charges nothing (the charge is reset below),
    // so no charge bound can stop it: the run's first statement meets the
    // bound instead (`K-CHG-002`).
    let bound = std::mem::replace(&mut machine.bounds.charge, u64::MAX);
    let (code, args) = match &start.target {
        Target::Main => {
            if !start.args.is_empty() {
                return Err(arguments("`main` takes no arguments".into()));
            }
            // The host's bindings are `main`'s over the session's, and
            // charge nothing: the cell did not make them.
            for (name, datum) in &start.host {
                let value = machine
                    .decode(&exe, datum, &lash_kernel_doc::Type::Any)
                    .map_err(|error| start_interrupt(name.as_str(), error))?;
                machine
                    .session_write(&BindingName::new(name.clone()), value, false)
                    .map_err(|error| start_interrupt(name.as_str(), error.into()))?;
                machine.host.push(name.clone());
            }
            machine.charged = 0;
            (exe.main, Vec::new())
        }
        Target::Entry(entry) => {
            if start.session.is_some() {
                return Err(StartError::Bindings {
                    problem: "an entry starts without a session".into(),
                });
            }
            let document = Arc::clone(&machine.program.document);
            let (Some(definition), Some(code)) =
                (document.entries.get(entry), exe.declared.get(entry))
            else {
                return Err(StartError::UnknownEntry {
                    entry: entry.clone(),
                });
            };
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
                            .decode(&exe, datum, &lash_kernel_doc::Type::Any)
                            .map_err(|error| start_interrupt("extra argument", error))?,
                    );
                }
                // Start arguments remain roots while the packed list reserves.
                machine.regs = args.clone().into();
                let list = machine
                    .alloc(ObjectInit::List(args))
                    .map_err(|error| start_interrupt("arguments", error.into()))?;
                args = vec![Value::Absent, Value::List(list)];
            }
            machine.charged = 0;
            (*code, args)
        }
    };
    machine.bounds.charge = bound;
    let count = args.len();
    machine.regs = args.into();
    machine
        .push_frame(TaskId::MAIN, &exe, exec::Call::new(code, 0, count))
        .map_err(|error| start_interrupt("the start", error))?;
    machine.regs.clear();
    machine
        .publish_roots()
        .map_err(|error| start_interrupt("the start", error.into()))?;
    machine.heap.release_statement_roots()?;
    Ok(machine)
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
        let release = lash_kernel_doc::heap::PageRef {
            id: self.release,
            grant: lash_kernel_doc::heap::PageGrant::NONE,
        };
        let saved = save.save(
            &mut self.heap,
            crate::paged::SaveCell {
                document: &document,
                release,
                bindings: &bindings,
                dropped,
                held: &held,
            },
        );
        self.save = Some(save);
        let saved = saved?;
        let mut functions = functions;
        for (name, record) in saved.functions {
            functions.write(
                lash_kernel_state::paged::metadata::metadata_key(&name),
                record,
                self.heap.pages(),
            )?;
        }
        let mut bindings: Tree<BindingRecord> = open_tree(carried);
        let mut kept = std::collections::BTreeSet::new();
        for (name, value) in saved.rebound {
            let record = self.binding_record(&name, value)?;
            bindings.write(
                lash_kernel_state::paged::metadata::metadata_key(&name),
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
        let mut work =
            lash_kernel_state::paged::tree::SealWork::new(lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            });
        functions.seal(self.heap.pages(), &mut work)?;
        bindings.seal(self.heap.pages(), &mut work)?;
        Ok((keyed(&bindings)?, keyed(&functions)?, not_carried))
    }

    /// The record of the session binding `name` holding `value`, declared at
    /// `main`.
    fn binding_record(&mut self, name: &Name, value: Value) -> Result<BindingRecord, Halt> {
        binding_record(
            &mut self.heap,
            &self.exe.code(self.exe.main).site,
            name,
            value,
        )
        .map_err(|error| self.heap_halt(error))
    }

    pub(super) fn session_get(&self, name: &BindingName) -> Result<Option<Value>, HeapFault> {
        self.heap.session_binding_at(&name.key)
    }
    pub(super) fn session_write(
        &mut self,
        name: &BindingName,
        value: Value,
        existing: bool,
    ) -> Result<bool, Halt> {
        // Declarations prepare their value before touching the binding tree.
        let record = if existing {
            None
        } else {
            Some(self.binding_record(name, value.clone())?)
        };
        let exe = Arc::clone(&self.exe);
        self.heap_write(|heap| {
            heap.write_session_binding(&name.key, |heap, previous| {
                if existing {
                    let Some(previous) = previous else {
                        return Ok(None);
                    };
                    // The former existence read resolved shared variables too.
                    if let BoundRecord::Shared(id) = previous.value {
                        heap.variable_get(id)?;
                    }
                }
                match &record {
                    Some(record) => Ok(Some(record.clone())),
                    None => binding_record(heap, &exe.code(exe.main).site, name, value.clone())
                        .map(Some),
                }
            })
            .map_err(super::write_halt)
        })
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
        self.heap.release_statement_holds()?;
        // The host's bindings are the host's: what the cell made of its
        // copy is not the session's.
        if !self.host.is_empty() {
            let mut bindings = self.heap.session_root().clone();
            for name in std::mem::take(&mut self.host) {
                bindings.remove(
                    &lash_kernel_state::paged::metadata::metadata_key(&name),
                    self.heap.pages(),
                )?;
            }
            self.heap
                .set_session_root(bindings)
                .map_err(|error| self.heap_halt(error))?;
        }
        // Lifecycle roots name sealed trees. A cell may finish before its
        // first park, so its session still has only transient pages here.
        self.heap
            .seal_session_root(&mut lash_kernel_state::paged::tree::SealWork::new(
                lash_kernel_state::paged::SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                },
            ))?;
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
pub(super) fn keyed<T: lash_kernel_state::paged::tree::Cell>(
    tree: &Tree<T>,
) -> Result<KeyedRoot, lash_kernel_doc::heap::HeapFault> {
    tree.keyed_root()
}

impl KernelMachine {
    pub fn start_with_layout(
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
            census,
            layout,
            lash_kernel_doc::KernelDataLimits::standard(),
        )
    }
    pub fn start_with_data_limits(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        limits: lash_kernel_doc::KernelDataLimits,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
    ) -> Result<Self, StartError> {
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
    declared: &lash_kernel_doc::Site,
    name: &Name,
    value: Value,
) -> Result<BindingRecord, HeapWriteError> {
    let value = heap.canonical_value(value)?;
    Ok(BindingRecord {
        name: lash_kernel_state::paged::NameRef::new(name, heap.pages())?,
        declared: lash_kernel_state::paged::SiteRef::new(declared, heap.pages())?,
        value: BoundRecord::Value(value),
    })
}
