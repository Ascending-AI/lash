//! A cell's session on the bytecode core (`K-SES-001`–`003`): the
//! bindings a cell starts with and the ones it hands on, as the roots of
//! a sealed object graph ([`run::session`]).
//!
//! A cell opens its session's heap cold: each binding's value is a root,
//! an object by proxy, so two bindings that shared an object share it
//! again (`K-SES-002`). A binding `main` names is its global; the others
//! are globals past the program's, so the collector, a seal and a reopen
//! keep them as they keep any global.
//!
//! A saved function's object is a global past the session's bindings, so
//! it stays rooted, and is handed on as it came.
//!
//! At the end, the session's bindings are the globals, then `main`'s
//! top-level variables, minus the host's. Their heap is sealed with them
//! and the saved functions' objects as its roots; a binding whose value
//! reaches a closure, a function or a task other than a saved function's
//! is dropped and named, and the heap sealed again without it.
//!
//! [`run::session`]: lash_kernel_state::paged::run::session

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NativeError, Retry};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::SessionRoot;
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::ObjectId;
use lash_kernel_state::paged::run::objects::{ObjectKind, Record, RunValue};
use lash_kernel_state::paged::run::session::{self, RunSession};
use lash_kernel_value::names::Name;

use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
use crate::interface::MachineError;
use crate::paged::SealError;
use crate::vm::Vm;
use crate::vm::tasks::MAIN;

/// The globals past the program's: the carried session's bindings `main`
/// names nowhere, ascending, then its saved functions, in that order.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    pub(crate) names: Vec<Name>,
    pub(crate) saved: Vec<session::RunFunction>,
}

/// What a session cell's end saves of the functions the bindings its
/// carry would drop reach (`K-SES-004`): the dialect's rules over the
/// sealed heap. The embedder that knows the cell's dialect supplies it.
pub(crate) trait Save {
    /// Saves what it can of `cell.dropped`.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    fn save(&mut self, cell: SaveCell<'_>) -> Result<SavedCell, HeapFault>;
}

/// A cell's end as its save reads it.
pub(crate) struct SaveCell<'a> {
    /// The sealed heap of every binding, the dropped ones included.
    pub(crate) directory: &'a Directory,
    /// Every binding the cell leaves, by name.
    pub(crate) bindings: &'a BTreeMap<Name, RunValue>,
    /// The bindings the carry drops, in name order.
    pub(crate) dropped: &'a [Name],
    /// The functions the session holds, with their objects.
    pub(crate) held: &'a [(ObjectId, session::RunFunction)],
}

/// What a cell's end saved.
#[derive(Debug, Default)]
pub(crate) struct SavedCell {
    /// Each function saved, with its object; one of a held function's
    /// name replaces it.
    pub(crate) functions: Vec<(ObjectId, session::RunFunction)>,
    /// The dropped bindings that carry after all.
    pub(crate) carried: Vec<Name>,
    /// Each dropped binding not carried, with why.
    pub(crate) refused: Vec<(Name, String)>,
}

/// Why a binding that reaches a closure, a function or a task is not
/// carried, when no dialect says more (`K-SES-003`).
pub(crate) const NOT_CARRIED: &str = "it holds a function or a task, which ends with its cell";

/// Binds `session`'s bindings, valued `values` (the heap's roots), in
/// `vm`: a name the program reads to its global, every other one past
/// them, then the saved functions' objects past those.
pub(crate) fn bind(vm: &mut Vm<SlotHeap>, session: &RunSession, values: Vec<Value>) {
    let (bindings, functions) = values.split_at(session.names.len().min(values.len()));
    for (name, value) in session.names.iter().zip(bindings) {
        match vm.program.globals.iter().position(|global| global == name) {
            Some(at) => vm.globals[at] = *value,
            None => {
                vm.globals.push(*value);
                vm.session.names.push(name.clone());
            }
        }
    }
    vm.globals.extend_from_slice(functions);
    vm.session.saved.clone_from(&session.functions);
}

/// Hands on `vm`'s session at the end of its cell, `host` the host's
/// bindings: the session root, and each binding dropped, with why. `save`
/// saves what it can of the functions the dropped bindings reach.
///
/// # Errors
///
/// [`MachineError::Heap`] when a page cannot be read or written;
/// [`MachineError::Fault`] for a heap that breaks an invariant.
pub(crate) fn carry(
    vm: &mut Vm<SlotHeap>,
    host: &[Name],
    save: Option<&mut (dyn Save + 'static)>,
) -> Result<(SessionRoot, Vec<(Name, String)>), MachineError> {
    let mut bindings = BTreeMap::new();
    let program = std::sync::Arc::clone(&vm.program);
    let named = program.globals.len() + vm.session.names.len();
    let mut functions = vm
        .globals
        .get(named..)
        .filter(|saved| saved.len() == vm.session.saved.len())
        .ok_or_else(|| fault("the run holds another number of saved functions"))?
        .to_vec();
    for (name, value) in program
        .globals
        .iter()
        .chain(&vm.session.names)
        .zip(&vm.globals)
    {
        bindings.insert(name.clone(), *value);
    }
    for (name, value) in top_level(vm)? {
        bindings.insert(name, value);
    }
    bindings.retain(|name, value| !matches!(value, Value::Absent) && !host.contains(name));
    let (names, mut values): (Vec<Name>, Vec<Value>) = bindings.into_iter().unzip();
    let bound = values.len();
    values.extend_from_slice(&functions);
    let mut heap = vm.heap.seal(&values).map_err(sealing)?;
    let directory = vm
        .heap
        .directory()
        .ok_or_else(|| fault("the heap has no directory"))?;
    let saved = heap.roots[bound..]
        .iter()
        .filter_map(RunValue::id)
        .collect();
    let reaching = Reach::new(directory, saved).classify(&heap.roots[..bound])?;
    let dropped: Vec<Name> = names
        .iter()
        .zip(&reaching)
        .filter(|(_, reaches)| **reaches)
        .map(|(name, _)| name.clone())
        .collect();
    let mut saved = SavedCell::default();
    if let Some(save) = save
        && !dropped.is_empty()
    {
        let bindings = names
            .iter()
            .cloned()
            .zip(heap.roots.iter().cloned())
            .collect();
        let held = heap.roots[bound..]
            .iter()
            .zip(&vm.session.saved)
            .map(|(root, function)| {
                root.id()
                    .map(|id| (id, function.clone()))
                    .ok_or_else(|| fault("a saved function's root is no object"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        saved = save
            .save(SaveCell {
                directory,
                bindings: &bindings,
                dropped: &dropped,
                held: &held,
            })
            .map_err(MachineError::Heap)?;
        let new = std::mem::take(&mut saved.functions)
            .into_iter()
            .map(|(id, function)| Ok((directory_kind(directory, id)?, id, function)))
            .collect::<Result<Vec<_>, MachineError>>()?;
        for (kind, id, function) in new {
            let value = sealed_function(vm, kind, id);
            match vm
                .session
                .saved
                .iter()
                .position(|held| held.name == function.name)
            {
                Some(at) => {
                    functions[at] = value;
                    vm.session.saved[at] = function;
                }
                None => {
                    functions.push(value);
                    vm.session.saved.push(function);
                }
            }
        }
    }
    let mut refused: BTreeMap<Name, String> = saved.refused.into_iter().collect();
    let mut not_carried = Vec::new();
    let mut kept = (Vec::new(), Vec::new());
    for ((name, value), reaches) in names.into_iter().zip(values).zip(reaching) {
        if reaches && !saved.carried.contains(&name) {
            let why = refused
                .remove(&name)
                .unwrap_or_else(|| NOT_CARRIED.to_owned());
            not_carried.push((name, why));
        } else {
            kept.0.push(name);
            kept.1.push(value);
        }
    }
    if kept.0.len() != bound || functions.len() != heap.roots.len() - bound {
        kept.1.extend_from_slice(&functions);
        heap = vm.heap.seal(&kept.1).map_err(sealing)?;
    }
    let directory = vm
        .heap
        .directory()
        .ok_or_else(|| fault("the heap has no directory"))?;
    let page = session::write(
        directory.access(),
        &RunSession {
            heap,
            names: kept.0,
            functions: vm.session.saved.clone(),
        },
    )
    .map_err(MachineError::Heap)?;
    Ok((SessionRoot::run(page), not_carried))
}

/// The kind of the closure or function object `id`.
fn directory_kind(directory: &Directory, id: ObjectId) -> Result<ObjectKind, MachineError> {
    match directory
        .get(id)
        .map_err(MachineError::Heap)?
        .map(|row| row.record)
    {
        Some(Record::Closure { .. }) => Ok(ObjectKind::Closure),
        Some(Record::Function(_)) => Ok(ObjectKind::Function),
        _ => Err(fault(format!("saved object {} is no function", id.0))),
    }
}

/// The value of the sealed function object `id`: the run's own handle.
fn sealed_function(vm: &mut Vm<SlotHeap>, kind: ObjectKind, id: ObjectId) -> Value {
    let handle = vm.heap.arena_mut().proxy(
        crate::heap::open::entity(id),
        lash_kernel_state::arena::ProxyKind::Unknown,
    );
    match kind {
        ObjectKind::Closure => Value::Closure(handle),
        _ => Value::Function(handle),
    }
}

/// `main`'s top-level variables and their values (`K-SES-001`): each one
/// a statement of its body declares and the document does not list as
/// private, read from its frame, or from its locals when it has returned.
/// A later declaration of a name stands over an earlier one once it ran.
fn top_level(vm: &mut Vm<SlotHeap>) -> Result<Vec<(Name, Value)>, MachineError> {
    let program = std::sync::Arc::clone(&vm.program);
    let unit = &program.units[program.main as usize];
    let slots = match &vm.top {
        Some(top) => top.clone(),
        None => {
            let cx = if vm.tasks.installed && vm.tasks.running == MAIN {
                &vm.cx
            } else {
                &vm.tasks.all[MAIN].cx
            };
            let Some(frame) = cx.frames.first().filter(|frame| frame.unit == program.main) else {
                return Ok(Vec::new());
            };
            let base = frame.base as usize;
            cx.stack
                .get(base..base + usize::from(unit.locals))
                .ok_or_else(|| fault("main's frame holds fewer locals than its unit"))?
                .to_vec()
        }
    };
    // A slot holds a cell when its unit boxes it at entry or where its
    // variable is declared (`K-CLO-001`).
    let cells: BTreeSet<u16> = unit
        .boxed
        .iter()
        .copied()
        .chain(unit.ops.iter().filter_map(|op| match op {
            crate::bytecode::op::Op::Box(slot) => Some(*slot),
            _ => None,
        }))
        .collect();
    let mut bound = Vec::new();
    for (slot, (path, name)) in unit.declared.iter().enumerate() {
        let Some(name) = name else { continue };
        if path.len() != 1 || vm.document.private_bindings.contains(name) {
            continue;
        }
        let Some(mut value) = slots.get(slot).copied() else {
            continue;
        };
        if cells.contains(&(slot as u16))
            && let Value::Handle(cell) = value
        {
            value = loop {
                match vm.heap.cell_get(cell) {
                    Ok(value) => break value,
                    Err(NativeError::Retry(Retry::Cold { object, chunk })) => vm
                        .reload(object, chunk)
                        .map_err(|stop| fault(format!("a cell does not load: {stop:?}")))?,
                    Err(error) => return Err(fault(error.to_string())),
                }
            };
        }
        if !matches!(value, Value::Absent) {
            bound.push((name.clone(), value));
        }
    }
    Ok(bound)
}

/// Which sealed values reach a closure, a function or a task other than a
/// saved function's, through the directory's rows: a leaf object is never
/// read, a large one's chunks are.
struct Reach<'a> {
    directory: &'a Directory,
    /// Objects that reach none: the saved functions', and every object
    /// found to.
    clean: BTreeSet<ObjectId>,
}

impl<'a> Reach<'a> {
    fn new(directory: &'a Directory, saved: BTreeSet<ObjectId>) -> Self {
        Self {
            directory,
            clean: saved,
        }
    }

    fn classify(&mut self, roots: &[RunValue]) -> Result<Vec<bool>, MachineError> {
        roots.iter().map(|root| self.reaches(root)).collect()
    }

    fn reaches(&mut self, root: &RunValue) -> Result<bool, MachineError> {
        let mut seen = BTreeSet::new();
        let mut work = Vec::new();
        if self.direct(root, &mut work) {
            return Ok(true);
        }
        while let Some(id) = work.pop() {
            if self.clean.contains(&id) || !seen.insert(id) {
                continue;
            }
            let row = self
                .directory
                .get(id)
                .map_err(MachineError::Heap)?
                .ok_or_else(|| fault(format!("object {} has no row", id.0)))?;
            let found = match &row.record {
                Record::Closure { .. } | Record::Function(_) => true,
                Record::Chunked(chunked) => {
                    let mut found = false;
                    crate::heap::large::any_value(self.directory.access(), chunked, &mut |value| {
                        found = found || self.direct(value, &mut work);
                        found
                    })
                    .map_err(MachineError::Heap)?;
                    found
                }
                record => record
                    .values()
                    .into_iter()
                    .any(|value| self.direct(value, &mut work)),
            };
            if found {
                return Ok(true);
            }
        }
        self.clean.extend(seen);
        Ok(false)
    }

    /// Whether `value` is itself a closure, a function or a task; else
    /// queues the object it names, when that object may hold one.
    fn direct(&self, value: &RunValue, work: &mut Vec<ObjectId>) -> bool {
        match value {
            RunValue::Task(_) | RunValue::TaskRef(_) => true,
            RunValue::Object(ObjectKind::Closure | ObjectKind::Function, id) => {
                !self.clean.contains(id)
            }
            RunValue::Object(
                ObjectKind::LongText
                | ObjectKind::Bytes
                | ObjectKind::BigInt
                | ObjectKind::Timestamp
                | ObjectKind::Handle,
                _,
            ) => false,
            RunValue::Object(_, id) | RunValue::Ref(id) => {
                if !self.clean.contains(id) {
                    work.push(*id);
                }
                false
            }
            _ => false,
        }
    }
}

fn fault(problem: impl Into<String>) -> MachineError {
    MachineError::Fault {
        problem: problem.into(),
    }
}

fn sealing(error: SealError) -> MachineError {
    match error {
        SealError::Heap(error) => MachineError::Heap(error),
        other => fault(other.to_string()),
    }
}
