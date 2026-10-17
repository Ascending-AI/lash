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
        Some(session) => PagedHeap::open_header(
            lash_kernel_state::kernel_internals::session_heap(session),
            source,
            EditEpoch(0),
        )?,
        None => PagedHeap::fresh(source, EditEpoch(0)),
    };
    heap.set_data_limits(limits);
    heap.set_census(census)?;
    heap.set_family_base(Box::new(
        lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
    ));
    let functions = start.session.as_ref().map_or_else(empty_root, |session| {
        lash_kernel_state::kernel_internals::session_functions(session)
    });
    if let Some(session) = &start.session {
        heap.adopt(session)?;
    }
    let access = heap.pages().clone();
    let arena: SharedArena = std::rc::Rc::new(std::cell::RefCell::new(
        lash_kernel_state::arena::Arena::from_ordinal(heap.next_ordinal()),
    ));
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
        session_cell: matches!(start.target, Target::Main),
        pins: Pins::default(),
        projecting: false,
        charging: true,
        region: None,
        slice_end: u64::MAX,
        sites: Default::default(),
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
        #[cfg(test)]
        boundary: None,
        #[cfg(test)]
        boundaries: 0,
    };
    machine.bind_modes(start.modes)?;
    machine.tasks.push(Task::fresh(
        lash_kernel_state::paged::TaskIdentityRef::MAIN,
        access,
        arena,
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
                    .new_object(NewObject::List(args))
                    .map_err(|error| start_interrupt("arguments", error.into()))?;
                args = vec![Value::Absent, list];
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
            &mut crate::CellSaveContext::new(&mut self.heap),
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
            &self.exe.code(self.exe.main).site,
            name,
            value,
        )
        .map_err(|error| self.heap_halt(error))
    }

    pub(super) fn session_get(&mut self, name: &BindingName) -> Result<Option<Value>, Halt> {
        let bound = self.heap_write(|heap| {
            heap.load_session_bound_at(&name.key)
                .map_err(super::write_halt)
        })?;
        Ok(match bound {
            None => None,
            Some(BoundRecord::Value(value)) => Some(self.admit_page(&value)?),
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
    pub(super) fn session_write(
        &mut self,
        name: &BindingName,
        value: Value,
        existing: bool,
    ) -> Result<bool, Halt> {
        // Declarations prepare their value before touching the binding tree.
        let value = self.page_form(&value)?;
        let record = if existing {
            None
        } else {
            Some(self.binding_record(name, value.clone())?)
        };
        let exe = Arc::clone(&self.exe);
        self.heap_write(|heap| {
            heap.write_session_binding(&name.key, |heap, previous| {
                if existing && previous.is_none() {
                    return Ok(None);
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
                .map_err(|error| self.heap_halt(error))?;
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
        let census = start_census(&mut *source, &start, bounds.memory)?;
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
        limits: lash_kernel_doc::KernelDataLimits,
        mut source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        let census = start_census(&mut *source, &start, bounds.memory)?;
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
            census,
            layout,
            lash_kernel_doc::KernelDataLimits::standard(),
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
/// heap's when none, held to the run's memory bound. Reads the header page
/// through `source`.
pub(crate) fn restored_census(
    source: &mut dyn PageSource,
    heap: Option<lash_kernel_doc::heap::PageRef>,
    memory: u64,
) -> Result<Box<dyn AllocationCensus>, AccountingFault> {
    use lash_kernel_state::paged::{AccountingCertificate, CensusFault, heap_accounting};
    let accounting = match heap {
        Some(heap) => heap_accounting(source, heap).map_err(AccountingFault::Heap)?,
        None => AccountingCertificate::FRESH,
    };
    lash_kernel_state::kernel_internals::census_impl::Census::restore(
        accounting,
        lash_kernel_doc::heap::LogicalBytes(memory),
    )
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
    memory: u64,
) -> Result<Box<dyn AllocationCensus>, AccountingFault> {
    restored_census(
        source,
        start
            .session
            .as_ref()
            .map(lash_kernel_state::kernel_internals::session_heap),
        memory,
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
    declared: &lash_kernel_doc::Site,
    name: &Name,
    value: PageValue,
) -> Result<BindingRecord, HeapWriteError> {
    Ok(BindingRecord {
        name: lash_kernel_state::paged::NameRef::new(name, heap.pages())?,
        declared: lash_kernel_state::paged::SiteRef::new(declared, heap.pages())?,
        value: BoundRecord::Value(value),
    })
}
