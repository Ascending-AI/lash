//! Starting and carrying a cell preserve the paged identity directory.
use super::*;
use crate::interface::{CellStart, Finished, Target};
use lash_kernel_doc::graph_tokens::{TokenCodec, encode_body};
use lash_kernel_state::paged::{AllocationCensus, CarryRoots, EditEpoch, KeyedRoot, PageSource};

pub(super) fn empty_root() -> KeyedRoot {
    KeyedRoot {
        page: None,
        len: 0,
        cost: LogicalBytes(0),
    }
}
pub(super) fn open_tree<T: lash_kernel_state::paged::tree::Cell>(root: KeyedRoot) -> Tree<T> {
    Tree::open(root.page, root.len, root.cost, false)
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
        let bindings = heap.adopt(session)?;
        heap.session = open_tree(bindings);
    }
    let access = heap.access.clone();
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
        withdrawn: queue::Queue::open(Tree::new(true), access.clone()),
        withdrawn_ever: Tree::new(false),
        modes: Tree::new(false),
        functions,
        session_cell: matches!(start.target, Target::Main),
        pins: Pins::default(),
        fresh: Vec::new(),
        charging: true,
        charged: 0,
        live_tasks: 1,
        inline_depth: 0,
        inline_result: None,
        regs: Vec::new(),
        top: 0,
        storage: Storage::default(),
        steps: Default::default(),
        end: None,
        ended: false,
        observer: None,
    };
    machine.bind_modes(start.modes)?;
    machine
        .tasks
        .push(Task::fresh(TaskIdentity::Main, access, Arc::clone(&exe)))?;
    let arguments = |problem: String| StartError::Arguments { problem };
    let (code, args) = match &start.target {
        Target::Main => {
            if !start.args.is_empty() {
                return Err(arguments("`main` takes no arguments".into()));
            }
            (exe.main, Vec::new())
        }
        Target::Entry(entry) => {
            if start.session.is_some() {
                return Err(StartError::Bindings {
                    problem: "an entry starts without a session".into(),
                });
            }
            let document = Arc::clone(&machine.program.document);
            let (Some(signature), Some(code)) =
                (document.entries.get(entry), exe.declared.get(entry))
            else {
                return Err(StartError::UnknownEntry {
                    entry: entry.clone(),
                });
            };
            let given = start.args.len();
            if !(signature.required()..=signature.params.len()).contains(&given) {
                return Err(arguments(format!(
                    "`{entry}` takes {} to {} argument(s); {given} given",
                    signature.required(),
                    signature.params.len()
                )));
            }
            let mut args = Vec::new();
            for (datum, param) in start.args.iter().zip(&signature.params) {
                args.push(if param.optional && matches!(datum, Datum::Absent) {
                    Value::Absent
                } else {
                    machine
                        .decode(&exe, datum, &param.ty)
                        .map_err(|error| start_interrupt(param.name.as_str(), error))?
                });
            }
            machine.charged = 0;
            (*code, args)
        }
    };
    let count = args.len();
    machine.regs = args;
    machine
        .push_frame(TaskId::MAIN, &exe, exec::Call::new(code, 0, count))
        .map_err(|error| start_interrupt("the start", error.into()))?;
    machine.regs.clear();
    machine
        .publish_roots()
        .map_err(|error| start_interrupt("the start", error.into()))?;
    machine.heap.release_statement_roots()?;
    Ok(machine)
}
impl KernelMachine {
    pub(super) fn session_get(&self, name: &Name) -> Result<Option<Value>, HeapFault> {
        self.heap
            .session
            .get(&encode_body(name), &self.heap.access)?
            .map(|binding| match binding.value {
                BoundRecord::Value(value) => Ok(value),
                BoundRecord::Shared(id) => self.heap.variable_get(id),
            })
            .transpose()
    }
    pub(super) fn session_write(&mut self, name: Name, value: Value) -> Result<(), Halt> {
        let value = self
            .heap
            .canonical_value(value)
            .map_err(|error| self.heap_halt(error))?;
        let mut bindings = self.heap.session.clone();
        bindings.write(
            encode_body(&name),
            BindingRecord {
                name,
                declared: self.exe.code(self.exe.main).site.clone(),
                value: BoundRecord::Value(value),
            },
            &self.heap.access,
        )?;
        self.heap
            .set_session_root(bindings)
            .map_err(|error| self.heap_halt(error))
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
        let bindings = keyed(&self.heap.session);
        let classification = self.heap.classify_carry(&bindings)?;
        let carry = self.heap.carry(CarryRoots {
            bindings: classification.carried,
            functions: self.functions,
        })?;
        Ok(Finished {
            result,
            finish,
            carry: Some(carry),
            not_carried: classification.dropped,
        })
    }
}
pub(super) fn keyed<T: lash_kernel_state::paged::tree::Cell>(tree: &Tree<T>) -> KeyedRoot {
    KeyedRoot {
        page: tree.page(),
        len: tree.len(),
        cost: LogicalBytes(tree.cost()),
    }
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
        Interrupt::Halt(Halt::Heap(error)) => StartError::Heap(error),
        other => StartError::Arguments {
            problem: format!("{what}: {other:?}"),
        },
    }
}
