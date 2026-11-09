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
//! At the end, the session's bindings are the globals, then `main`'s
//! top-level variables, minus the host's. Their heap is sealed with them
//! as its roots; a binding whose value reaches a closure, a function or a
//! task is dropped and named, and the heap sealed again without it.
//!
//! [`run::session`]: lash_kernel_state::paged::run::session

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NativeError, Retry};
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

/// Why a binding that reaches a closure, a function or a task is not
/// carried, when no dialect says more (`K-SES-003`).
pub(crate) const NOT_CARRIED: &str = "it holds a function or a task, which ends with its cell";

/// Binds `session`'s bindings, valued `values`, in `vm`: a name the
/// program reads to its global, every other one past them.
pub(crate) fn bind(vm: &mut Vm<SlotHeap>, session: &RunSession, values: Vec<Value>) {
    for (name, value) in session.names.iter().zip(values) {
        match vm.program.globals.iter().position(|global| global == name) {
            Some(at) => vm.globals[at] = value,
            None => {
                vm.globals.push(value);
                vm.session.push(name.clone());
            }
        }
    }
}

/// Hands on `vm`'s session at the end of its cell, `host` the host's
/// bindings: the session root, and each binding dropped, with why.
///
/// # Errors
///
/// [`MachineError::Heap`] when a page cannot be read or written;
/// [`MachineError::Fault`] for a heap that breaks an invariant.
pub(crate) fn carry(
    vm: &mut Vm<SlotHeap>,
    host: &[Name],
) -> Result<(SessionRoot, Vec<(Name, String)>), MachineError> {
    let mut bindings = BTreeMap::new();
    let program = std::sync::Arc::clone(&vm.program);
    for (name, value) in program.globals.iter().chain(&vm.session).zip(&vm.globals) {
        bindings.insert(name.clone(), *value);
    }
    for (name, value) in top_level(vm)? {
        bindings.insert(name, value);
    }
    bindings.retain(|name, value| !matches!(value, Value::Absent) && !host.contains(name));
    let (names, values): (Vec<Name>, Vec<Value>) = bindings.into_iter().unzip();
    let mut heap = vm.heap.seal(&values).map_err(sealing)?;
    let directory = vm
        .heap
        .directory()
        .ok_or_else(|| fault("the heap has no directory"))?;
    let reaching = Reach::new(directory).classify(&heap.roots)?;
    let mut not_carried = Vec::new();
    let mut kept = (Vec::new(), Vec::new());
    for ((name, value), reaches) in names.into_iter().zip(values).zip(reaching) {
        if reaches {
            not_carried.push((name, NOT_CARRIED.to_owned()));
        } else {
            kept.0.push(name);
            kept.1.push(value);
        }
    }
    if !not_carried.is_empty() {
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
        },
    )
    .map_err(MachineError::Heap)?;
    Ok((SessionRoot::run(page), not_carried))
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
    let mut bound = Vec::new();
    for (slot, (path, name)) in unit.declared.iter().enumerate() {
        let Some(name) = name else { continue };
        if path.len() != 1 || vm.document.private_bindings.contains(name) {
            continue;
        }
        let Some(mut value) = slots.get(slot).copied() else {
            continue;
        };
        if unit.boxed.contains(&(slot as u16))
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

/// Which sealed values reach a closure, a function or a task, through the
/// directory's rows: a leaf object is never read, a large one's chunks
/// are.
struct Reach<'a> {
    directory: &'a Directory,
    /// Objects that reach none.
    clean: BTreeSet<ObjectId>,
}

impl<'a> Reach<'a> {
    fn new(directory: &'a Directory) -> Self {
        Self {
            directory,
            clean: BTreeSet::new(),
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
            RunValue::Object(ObjectKind::Closure | ObjectKind::Function, _) => true,
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
