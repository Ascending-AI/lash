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
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::native::{NativeError, Retry};
use lash_kernel_doc::heap::{HeapFault, PageRef};
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
use crate::vm::tasks::MAIN;
use crate::vm::{Flow, Stop, Vm};

/// The globals past the program's: the carried session's bindings `main`
/// names nowhere, ascending, then its saved functions, in that order; and
/// the session page they came from, whose rows the cell's carry edits.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    pub(crate) names: Vec<Name>,
    pub(crate) saved: Vec<session::RunFunction>,
    pub(crate) page: Option<PageRef>,
}

/// What a session cell's end saves of the functions the bindings its
/// carry would drop reach (`K-SES-004`): the dialect's rules over the
/// sealed heap. The embedder that knows the cell's dialect supplies it.
pub trait CellSave {
    /// Saves what it can of `cell`'s dropped bindings, each saved
    /// function with the bytes its dialect describes it in.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    fn save(&mut self, cell: SaveCell<'_>) -> Result<SavedCell, HeapFault>;
}

/// A cell's end as its save reads it: the sealed heap of every binding,
/// the dropped ones included, and the functions the session holds.
pub struct SaveCell<'a> {
    directory: &'a Directory,
    bindings: &'a BTreeMap<Name, RunValue>,
    dropped: &'a [Name],
    held: &'a [(ObjectId, session::RunFunction)],
}

/// The dialect's rules a cell's end saves under, beside what the cell
/// itself leaves.
pub struct SaveRules<'a> {
    /// The cell's document and the Link it ran under.
    pub document: &'a lash_kernel_doc::Document,
    pub link: lash_kernel_value::content_ids::LinkId,
    /// Every other document the cell's code reached, by its Link.
    pub documents: &'a BTreeMap<lash_kernel_value::content_ids::LinkId, lash_kernel_doc::Document>,
    pub controls: &'a BTreeMap<
        lash_kernel_value::names::OperationUse,
        BTreeSet<lash_kernel_dialect::EffectControl>,
    >,
    /// What the cell's source wrote of each function its document makes,
    /// by the site of its closure.
    pub written: &'a BTreeMap<lash_kernel_value::sites::Site, lash_kernel_dialect::Written>,
    /// The dialect's function values.
    pub values: &'a lash_kernel_doc::FunctionRepresentation,
    /// The library the cell ran against.
    pub catalog: &'a dyn lash_kernel_doc::FunctionCatalog,
}

/// A function a cell's end saved: its code and what it requires, and
/// the session root it is saved as.
#[derive(Clone, Debug)]
pub struct SavedFunction {
    pub name: Name,
    pub link: lash_kernel_value::content_ids::LinkId,
    pub callable: lash_kernel_value::names::CallableId,
    pub document: lash_kernel_doc::IdentifiedDocument,
    /// The session bindings its code reads by name.
    pub references: BTreeSet<Name>,
    pub written: Option<lash_kernel_dialect::Written>,
    /// The tag of the token a binding holds it in, when one does.
    pub token: Option<String>,
    pub held: Option<lash_kernel_dialect::HeldAt>,
    object: ObjectId,
}

/// What the dialect's rules save of a cell's end.
#[derive(Debug, Default)]
pub struct Saving {
    pub functions: Vec<SavedFunction>,
    /// The dropped bindings that carry as they are.
    pub carried: BTreeSet<Name>,
    /// Each dropped binding not carried, with why.
    pub refused: BTreeMap<Name, lash_kernel_dialect::NotSaved>,
}

/// What a cell's end saved.
#[derive(Debug, Default)]
pub struct SavedCell {
    /// Each function saved, with its description; one of a held
    /// function's name replaces it.
    pub functions: Vec<(SavedFunction, Vec<u8>)>,
    /// The dropped bindings that carry after all.
    pub carried: Vec<Name>,
    /// Each dropped binding not carried, with why.
    pub refused: Vec<(Name, String)>,
}

impl SaveCell<'_> {
    /// The bindings the carry drops, in name order.
    #[must_use]
    pub fn dropped(&self) -> &[Name] {
        self.dropped
    }

    /// Whether the cell leaves a binding `name`.
    #[must_use]
    pub fn binds(&self, name: &Name) -> bool {
        self.bindings.contains_key(name)
    }

    /// Every closure the dropped bindings reach, saved once under
    /// `rules`, the dropped bindings that carry as they are, and why each
    /// other one does not (`K-SES-004`).
    ///
    /// # Errors
    ///
    /// The heap's faults.
    pub fn save(&self, rules: SaveRules<'_>) -> Result<Saving, HeapFault> {
        let held = self
            .held
            .iter()
            .map(|(_, function)| function.name.clone())
            .collect();
        let saved = lash_kernel_dialect::kernel_internals::save_run(
            self.directory,
            lash_kernel_dialect::kernel_internals::RunLeft {
                document: rules.document,
                link: rules.link,
                documents: rules.documents,
                bindings: self.bindings,
                not_carried: self.dropped,
                controls: rules.controls,
                written: rules.written,
                values: rules.values,
                catalog: rules.catalog,
            },
            &held,
        )?;
        Ok(Saving {
            functions: saved
                .functions
                .into_values()
                .map(|function| SavedFunction {
                    name: function.name,
                    link: function.link,
                    callable: function.callable,
                    document: function.document,
                    references: function.references,
                    written: function.written,
                    token: function.token,
                    held: function.held,
                    object: function.object,
                })
                .collect(),
            carried: saved.carried,
            refused: saved.refused,
        })
    }
}

/// Why a binding that reaches a closure, a function or a task is not
/// carried, when no dialect says more (`K-SES-003`).
pub(crate) const NOT_CARRIED: &str = "it holds a function or a task, which ends with its cell";

/// Binds `session`'s bindings, valued `values` (the heap's roots), in
/// `vm`: a name the program reads to its global, every other one past
/// them, then the saved functions' objects past those; `page` is the
/// session page they came from.
pub(crate) fn bind(vm: &mut Vm<SlotHeap>, session: &RunSession, values: Vec<Value>, page: PageRef) {
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
    vm.session.page = Some(page);
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
    save: Option<&mut (dyn CellSave + 'static)>,
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
    // A module binding the cell declared and never assigned carries
    // unbound (`K-SES-001`): the next cell names it, and reading it raises.
    for (name, value) in top_level(vm)? {
        if matches!(value, Value::Absent) {
            bindings.entry(name).or_insert(value);
        } else {
            bindings.insert(name, value);
        }
    }
    bindings.retain(|name, _| !host.contains(name));
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
    let reaching = Reach::new(vm.heap.arena(), directory, saved).classify(&heap.roots[..bound])?;
    let dropped: Vec<Name> = names
        .iter()
        .zip(&reaching)
        .filter(|(_, reaches)| **reaches)
        .map(|(name, _)| name.clone())
        .collect();
    let mut saved = SavedCell::default();
    let mut resave = false;
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
            .map(|(function, description)| {
                let id = function.object;
                let function = session::RunFunction {
                    name: function.name,
                    description,
                };
                Ok((directory_kind(directory, id)?, id, function))
            })
            .collect::<Result<Vec<_>, MachineError>>()?;
        for (kind, id, function) in new {
            let value = sealed_function(vm, kind, id);
            if let Value::Closure(closure) = value {
                freeze(vm, closure)?;
            }
            // The session holds its functions in name order.
            match vm
                .session
                .saved
                .binary_search_by(|held| held.name.cmp(&function.name))
            {
                Ok(at) => {
                    functions[at] = value;
                    vm.session.saved[at] = function;
                }
                Err(at) => {
                    functions.insert(at, value);
                    vm.session.saved.insert(at, function);
                }
            }
            resave = true;
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
    if resave || kept.0.len() != bound {
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
        vm.session.page,
    )
    .map_err(MachineError::Heap)?;
    Ok((SessionRoot::run(page), not_carried))
}

/// Freezes newly saved `closure`'s captures as its cell left them
/// (FIG-5778), loading what of them is cold.
fn freeze(vm: &mut Vm<SlotHeap>, closure: ArenaHandle) -> Result<(), MachineError> {
    loop {
        match vm.freeze(closure) {
            Ok(()) => return Ok(()),
            Err(Flow::Cold { object, chunk }) => vm.reload(object, chunk).map_err(stopped)?,
            Err(Flow::Stop(stop)) => return Err(stopped(*stop)),
            Err(Flow::Raise(_)) => {
                return Err(fault("a saved function's capture reaches itself"));
            }
        }
    }
}

/// What a stop while freezing captures is to the carry.
fn stopped(stop: Stop) -> MachineError {
    match stop {
        Stop::Host(fault) => MachineError::Host(fault),
        Stop::Fault(problem) => fault(problem),
        Stop::Bound(exceeded) => fault(format!(
            "freezing a saved function's captures passed the {:?} bound",
            exceeded.bound
        )),
        _ => fault("freezing a saved function's captures stopped the run"),
    }
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
    let unit = program.unit(program.main);
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
        bound.push((name.clone(), value));
    }
    Ok(bound)
}

/// Which sealed values reach a closure, a function or a task other than a
/// saved function's: a resident object is read in the arena, a cold one
/// through its directory row (a leaf object is never read, a large one's
/// chunks are), so carrying a resident graph reads none of it back.
struct Reach<'a> {
    arena: &'a lash_kernel_state::arena::Arena,
    directory: &'a Directory,
    /// Objects that reach none: the saved functions', and every object
    /// found to.
    clean: BTreeSet<ObjectId>,
}

impl<'a> Reach<'a> {
    fn new(
        arena: &'a lash_kernel_state::arena::Arena,
        directory: &'a Directory,
        saved: BTreeSet<ObjectId>,
    ) -> Self {
        Self {
            arena,
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
            if let Some(found) = self.resident(id, &mut work) {
                if found {
                    return Ok(true);
                }
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

    /// Whether object `id`, held whole in the arena, is or directly holds a
    /// closure, a function or a task, queueing the objects it names; none
    /// when the arena holds it cold or in part, for its row to answer.
    fn resident(&self, id: ObjectId, work: &mut Vec<ObjectId>) -> Option<bool> {
        use lash_kernel_doc::arena::{Body, Chunks};
        fn whole(chunks: &Chunks) -> bool {
            (0..chunks.count()).all(|chunk| chunks.resident(chunk).is_some())
        }
        let handle = self.arena.entity_handle(crate::heap::open::entity(id))?;
        if self.arena.facts(handle).ok()?.cold {
            return None;
        }
        let body = self.arena.body(handle).ok()?;
        let whole = match body {
            Body::List(sequence) | Body::Tuple(sequence) => whole(&sequence.chunks),
            Body::Map(table) | Body::Set(table) | Body::Record(table) => whole(&table.chunks),
            Body::Closure { .. } | Body::Function(_) => return Some(true),
            Body::Error(_) | Body::Cell(_) => true,
            _ => return None,
        };
        if !whole {
            return None;
        }
        let mut found = false;
        let mut unnamed = false;
        lash_kernel_state::arena::each_member(body, &mut |value| {
            if found || unnamed {
                return;
            }
            match self.member(value) {
                Some(Member::Reaches) => found = true,
                Some(Member::Object(id)) => work.push(id),
                Some(Member::Leaf) => {}
                None => unnamed = true,
            }
        });
        if unnamed { None } else { Some(found) }
    }

    /// What resident member `value` is to the walk; none for an object the
    /// last seal gave no id.
    fn member(&self, value: &Value) -> Option<Member> {
        let id = |handle: &ArenaHandle| {
            let facts = self.arena.facts(*handle).ok()?;
            match facts.entity()? {
                lash_kernel_doc::heap::EntityId {
                    family: lash_kernel_doc::heap::FamilyId::PRIMARY,
                    ordinal,
                } => Some(ObjectId(ordinal)),
                _ => None,
            }
        };
        Some(match value {
            Value::Task(_) | Value::TaskRef(_) => Member::Reaches,
            Value::Closure(handle) | Value::Function(handle) => {
                if self.clean.contains(&id(handle)?) {
                    Member::Leaf
                } else {
                    Member::Reaches
                }
            }
            Value::LongText(_) | Value::Bytes(_) | Value::BigInt(_) | Value::BigTimestamp(_) => {
                Member::Leaf
            }
            Value::Handle(handle)
                if !matches!(
                    self.arena.body(*handle),
                    Ok(lash_kernel_doc::arena::Body::Cell(_))
                ) =>
            {
                Member::Leaf
            }
            Value::Handle(handle)
            | Value::Tuple(handle)
            | Value::Error(handle)
            | Value::List(handle)
            | Value::Map(handle)
            | Value::Set(handle)
            | Value::Record(handle)
            | Value::ObjectRef(handle) => {
                let id = id(handle)?;
                if self.clean.contains(&id) {
                    Member::Leaf
                } else {
                    Member::Object(id)
                }
            }
            _ => Member::Leaf,
        })
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

/// A resident member, as the reach walk takes it.
enum Member {
    /// A closure, a function or a task other than a saved function.
    Reaches,
    /// An object that may hold one.
    Object(ObjectId),
    /// Nothing that can hold one.
    Leaf,
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
