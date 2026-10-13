//! One session's execution state: its bindings (`K-SES-001`).
//!
//! Each cell is its own kernel document. What a session carries from one
//! cell to the next is the bindings `main` left at its top level, as data:
//! every variable's value and the heap objects those values reach. The
//! parent holds them between cells, hands them to the next cell's machine
//! as its start, and takes what that cell leaves.
//!
//! A binding that reaches a closure or a task handle is not carried
//! (`K-SES-003`). A function a cell bound to a name of its own is kept
//! instead as a saved function: its code and the variables it read, frozen
//! as data when the cell ended (`lash_kernel_dialect::SavedFunction`). The
//! session holds those beside its bindings, each once however many
//! bindings hold it, a later cell that names one has it declared in its
//! own document, and in that cell each of its bindings is a reference to
//! the declaration. Any other such binding is dropped, and
//! the session remembers its name and the reason, so a later cell that
//! reads it is told why it is gone.
//!
//! The state is saved as one checkpoint graph ([`graph`]): a complete
//! SessionExecution root whose large text and bytes are shared leaves and
//! whose saved functions are immutable bundles, shared by every state that
//! holds them. A capture writes the whole root and sends only the nodes no
//! acknowledged capture installed. The session renumbers the objects a cell
//! leaves into each binding's own range ([`SessionBindings::settle`]): that
//! numbering is the session's heap bookkeeping, never a storage identity.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_core_store::execution_state::{
    ExecutionGraphCapture, ExecutionGraphPurpose, HydratedExecutionGraph,
};
use lash_kernel_dialect::{FunctionValues, Kept, NotSaved, SavedFunction};
use lash_kernel_doc::{
    Annotations, Document, Identity, KERNEL_VERSION, Name, NumberPolicy, Object, ObjectId, Value,
};
use lash_kernel_vm::Bindings;

use super::snapshot::CodeModeSnapshotError;

mod graph;
mod release;

/// The reserved binding the history projection answers; no cell binds it.
pub(crate) const HISTORY_BINDING: &str = "history";

/// The objects one binding may own: its identities are its slot's range.
const SLOT_OBJECTS: u64 = 1 << 32;

/// A session's bindings as the parent holds them between cells.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SessionBindings {
    variables: BTreeMap<Name, Value>,
    objects: BTreeMap<ObjectId, Object>,
    /// The range of identities each binding's own objects take. A binding
    /// keeps its slot for as long as the session holds it, so its fragment
    /// changes only when the binding does.
    slots: BTreeMap<Name, u32>,
    next_slot: u32,
    /// The bindings a cell left that were not carried (`K-SES-003`), each
    /// with why, until a later cell binds the name again.
    not_carried: BTreeMap<Name, NotSaved>,
    /// The functions the session holds, each by the binding it is declared
    /// under ([`SavedFunction::name`]).
    functions: BTreeMap<Name, HeldFunction>,
    /// How many cells have left this session their bindings.
    cells: u64,
}

/// A saved function as a session holds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HeldFunction {
    pub(crate) function: SavedFunction,
    /// The session's cell, counted from one, whose end froze the
    /// function's captures: its generation. `None` for a function the
    /// session was created with.
    pub(crate) cell: Option<u64>,
    /// The other bindings that hold the function, besides the one it is
    /// declared under: the one function, which a cell makes once.
    pub(crate) aliases: BTreeSet<Name>,
}

/// Each binding of `functions` with the function it holds.
fn bound(functions: &BTreeMap<Name, HeldFunction>) -> BTreeMap<&Name, &HeldFunction> {
    functions
        .iter()
        .flat_map(|(name, held)| {
            std::iter::once(name)
                .chain(&held.aliases)
                .map(move |binding| (binding, held))
        })
        .collect()
}

/// Refuses held functions whose bindings are not their own: one declared
/// under a name other than its key, or a binding held twice or also held
/// as data.
fn check_held(
    functions: &BTreeMap<Name, HeldFunction>,
    data: &BTreeMap<Name, Value>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (name, held) in functions {
        if held.function.name != *name {
            return Err(format!(
                "the function held as `{name}` is declared as `{}`",
                held.function.name
            ));
        }
        for binding in std::iter::once(&held.function.name).chain(&held.aliases) {
            if !seen.insert(binding) || data.contains_key(binding) {
                return Err(format!("`{binding}` is bound twice"));
            }
        }
    }
    Ok(())
}

/// What a finished cell left a session.
pub(super) struct CellLeft<'a> {
    pub document: &'a Document,
    pub annotations: Option<&'a Annotations>,
    /// `main`'s top-level bindings that are data.
    pub bindings: Bindings,
    /// The ones the run's end did not carry, and of those the ones that
    /// reach a closure and no task.
    pub not_carried: Vec<Name>,
    pub closures: Bindings,
    /// The effects the cell was lowered against whose call ends the turn.
    pub controls:
        BTreeMap<lash_kernel_doc::EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
    /// What the cell's dialect declared its function values are; `None`
    /// when its sessions keep no function between cells.
    pub function_values: Option<&'a FunctionValues>,
}

impl SessionBindings {
    /// The bindings a cell's machine starts with.
    pub(crate) fn start(&self) -> Bindings {
        let mut variables = self.variables.clone();
        // A saved function is, in a cell, a reference to its declaration
        // in that cell's document, under each of its bindings.
        for (name, held) in bound(&self.functions) {
            variables.insert(name.clone(), Value::Function(held.function.name.clone()));
        }
        Bindings {
            variables,
            objects: self.objects.clone(),
        }
    }

    /// Every name in scope when a cell's `main` starts: the bindings and
    /// the functions the session holds. A name it lost to `K-SES-003` is
    /// not among them.
    pub(crate) fn names(&self) -> BTreeSet<Name> {
        self.variables
            .keys()
            .chain(bound(&self.functions).into_keys())
            .cloned()
            .collect()
    }

    /// The functions a cell is lowered against, by each binding that holds
    /// one.
    pub(crate) fn functions(&self) -> BTreeMap<Name, SavedFunction> {
        bound(&self.functions)
            .into_iter()
            .map(|(name, held)| (name.clone(), held.function.clone()))
            .collect()
    }

    /// The functions the session holds, by each binding that holds one.
    pub(crate) fn held_functions(&self) -> BTreeMap<&Name, &HeldFunction> {
        bound(&self.functions)
    }

    /// Unbinds `name` from the function it holds, if it holds one: the
    /// function's other bindings keep it, declared under the first of them
    /// when `name` was the one it was declared under.
    fn unbind(&mut self, name: &Name) {
        if let Some(held) = self
            .functions
            .values_mut()
            .find(|held| held.aliases.contains(name))
        {
            held.aliases.remove(name);
            return;
        }
        let Some(mut held) = self.functions.remove(name) else {
            return;
        };
        if let Some(first) = held.aliases.pop_first() {
            held.function = held.function.renamed(&first);
            self.functions.insert(first, held);
        }
    }

    pub(crate) fn not_carried(&self) -> &BTreeMap<Name, NotSaved> {
        &self.not_carried
    }

    /// Takes what a cell left. A function bound to names of its own whose
    /// captures are data or other saved functions becomes one saved
    /// function, which each of those names holds; the bindings that hold a
    /// reference to a saved function, bare or in its dialect's token, keep
    /// the function it names, with that token. Where the cell's dialect
    /// keeps no function between cells (`function_values`), none is saved.
    /// Answers what that did to the bindings, every name: a saved function
    /// is a binding like any other.
    pub(super) fn settle(&mut self, cell: CellLeft<'_>) -> lash_core::BindingChanges {
        self.cells += 1;
        let held: BTreeSet<Name> = self.functions.keys().cloned().collect();
        let (saved, refused) = if let Some(values) = cell.function_values {
            lash_kernel_dialect::save(
                lash_kernel_dialect::Left {
                    document: cell.document,
                    carried: &cell.bindings.variables,
                    carried_objects: &cell.bindings.objects,
                    closures: &cell.closures.variables,
                    closure_objects: &cell.closures.objects,
                    not_carried: &cell.not_carried,
                    controls: &cell.controls,
                    annotations: cell.annotations,
                    values,
                },
                &held,
            )
        } else {
            let refused = cell
                .not_carried
                .iter()
                .map(|name| {
                    let why = if cell.closures.variables.contains_key(name) {
                        NotSaved::Dialect
                    } else {
                        NotSaved::Task
                    };
                    (name.clone(), why)
                })
                .collect();
            (BTreeMap::new(), refused)
        };
        let mut left = cell.bindings;
        // A reference to a saved function is that function, under whichever
        // names hold it now, in the token it is held in.
        let values = cell.function_values.unwrap_or(&FunctionValues::Bare);
        let mut holding: BTreeMap<Name, (BTreeSet<Name>, Option<lash_kernel_dialect::Token>)> =
            BTreeMap::new();
        left.variables.retain(|name, value| {
            let Some(target) = lash_kernel_dialect::function_reference(value, values)
                .filter(|target| self.functions.contains_key(*target))
            else {
                return true;
            };
            let (names, token) = holding.entry(target.clone()).or_default();
            names.insert(name.clone());
            if token.is_none() {
                *token = lash_kernel_dialect::token_of(value, &left.objects, values);
            }
            false
        });
        let mut functions = BTreeMap::new();
        for (target, (mut names, token)) in holding {
            let Some(held) = self.functions.get(&target) else {
                continue;
            };
            // Declared under its name while a binding of that name holds
            // it, and otherwise under the first that does.
            let declared = if names.remove(&target) {
                target
            } else {
                let Some(first) = names.pop_first() else {
                    continue;
                };
                first
            };
            let mut function = held.function.renamed(&declared);
            if token.is_some() {
                function.token = token;
            }
            functions.insert(
                declared,
                HeldFunction {
                    function,
                    cell: held.cell,
                    aliases: names,
                },
            );
        }
        for (name, Kept { function, aliases }) in saved {
            functions.insert(
                name,
                HeldFunction {
                    function,
                    cell: Some(self.cells),
                    aliases,
                },
            );
        }
        let now_bound: BTreeSet<Name> = bound(&functions).into_keys().cloned().collect();
        self.not_carried
            .retain(|name, _| !left.variables.contains_key(name) && !now_bound.contains(name));
        let before = Bindings {
            variables: std::mem::take(&mut self.variables),
            objects: std::mem::take(&mut self.objects),
        };
        let before_held = std::mem::replace(&mut self.functions, functions);
        let before_functions = bound(&before_held);
        self.adopt(left);
        let mut changes = lash_core::BindingChanges {
            not_carried: refused.keys().map(ToString::to_string).collect(),
            ..Default::default()
        };
        for (name, value) in &self.variables {
            match before.variables.get(name) {
                // The name held a function and now holds data.
                None if before_functions.contains_key(name) => {
                    changes.changed.push(name.to_string());
                }
                None => changes.added.push(name.to_string()),
                Some(was)
                    if binding_data(was, &before.objects) != binding_data(value, &self.objects) =>
                {
                    changes.changed.push(name.to_string());
                }
                Some(_) => {}
            }
        }
        let after_functions = bound(&self.functions);
        for (name, held) in &after_functions {
            match before_functions.get(*name) {
                Some(was) if was.function == held.function => {}
                None if !before.variables.contains_key(*name) => {
                    changes.added.push(name.to_string());
                }
                _ => changes.changed.push(name.to_string()),
            }
        }
        changes.added.sort();
        changes.changed.sort();
        changes.removed = before
            .variables
            .keys()
            .chain(before_functions.keys().copied())
            .filter(|name| {
                !self.variables.contains_key(*name)
                    && !after_functions.contains_key(*name)
                    && !refused.contains_key(*name)
            })
            .map(ToString::to_string)
            .collect();
        self.not_carried.extend(refused);
        changes
    }

    /// Records that `function`, stored in an earlier kernel version under
    /// the bindings `names`, was refused by the migration to this build's:
    /// the session holds no function under them, and lists each with why.
    fn not_migrated(
        &mut self,
        names: impl IntoIterator<Item = Name>,
        function: &SavedFunction,
        refusal: &lash_vm_runtime::KernelMigrationRefusal,
    ) {
        for name in names {
            self.unbind(&name);
            self.not_carried.insert(
                name,
                NotSaved::NotMigrated {
                    from: function.document.manifest.kernel,
                    problem: refusal.to_string(),
                },
            );
        }
    }

    /// Holds `function` under `name`, as a session is created with it.
    fn hold(&mut self, name: Name, function: SavedFunction) {
        let function = function.renamed(&name);
        self.unbind(&name);
        self.not_carried.remove(&name);
        let mut left = Bindings {
            variables: self.variables.clone(),
            objects: self.objects.clone(),
        };
        if left.variables.remove(&name).is_some() {
            self.adopt(left);
        }
        self.functions.insert(
            name,
            HeldFunction {
                function,
                cell: None,
                aliases: BTreeSet::new(),
            },
        );
    }

    /// Replaces the bindings with `left`, renumbering its objects: each is
    /// owned by the first binding, in name order, that reaches it, and
    /// takes the next identity of that binding's slot in the order the walk
    /// meets it.
    fn adopt(&mut self, left: Bindings) {
        self.slots
            .retain(|name, _| left.variables.contains_key(name));
        let mut renamed: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for (name, value) in &left.variables {
            let slot = match self.slots.get(name) {
                Some(slot) => *slot,
                None => {
                    let slot = self.next_slot;
                    self.next_slot += 1;
                    self.slots.insert(name.clone(), slot);
                    slot
                }
            };
            let mut next = u64::from(slot) * SLOT_OBJECTS;
            let mut pending = Vec::new();
            value_objects(value, &mut pending);
            // Depth first, in the order the value names its objects.
            while let Some(id) = pending.pop() {
                if renamed.contains_key(&id) {
                    continue;
                }
                renamed.insert(id, ObjectId(next));
                next += 1;
                if let Some(object) = left.objects.get(&id) {
                    let mut named = Vec::new();
                    object_objects(object, &mut named);
                    pending.extend(named.into_iter().rev());
                }
            }
        }
        let rename = |id: &ObjectId| renamed.get(id).copied().unwrap_or(*id);
        self.variables = left
            .variables
            .iter()
            .map(|(name, value)| (name.clone(), rename_value(value, &rename)))
            .collect();
        self.objects = left
            .objects
            .iter()
            .filter_map(|(id, object)| {
                renamed
                    .get(id)
                    .map(|renamed| (*renamed, rename_object(object, &rename)))
            })
            .collect();
    }

    /// Binds `name` to a host's JSON `value`, decoded by `numbers`.
    fn seed(&mut self, name: &str, value: &serde_json::Value, numbers: NumberPolicy) {
        let mut left = self.data();
        let mut next = left
            .objects
            .keys()
            .next_back()
            .map_or(0, |id| id.0.saturating_add(1));
        let mut allocate = || {
            let id = ObjectId(next);
            next += 1;
            id
        };
        let value = crate::cell_value::bind_json(value, numbers, &mut allocate, &mut left.objects);
        left.variables.insert(Name::new(name), value);
        self.not_carried.remove(&Name::new(name));
        self.unbind(&Name::new(name));
        self.adopt(left);
    }

    /// The session's data bindings alone.
    fn data(&self) -> Bindings {
        Bindings {
            variables: self.variables.clone(),
            objects: self.objects.clone(),
        }
    }

    fn remove(&mut self, names: &BTreeSet<String>) -> bool {
        let mut left = self.data();
        let held = |bindings: &Self| bound(&bindings.functions).len();
        let before = left.variables.len() + self.not_carried.len() + held(self);
        left.variables
            .retain(|name, _| !names.contains(name.as_str()));
        self.not_carried
            .retain(|name, _| !names.contains(name.as_str()));
        for name in names {
            self.unbind(&Name::new(name.as_str()));
        }
        let changed = left.variables.len() + self.not_carried.len() + held(self) != before;
        if changed {
            self.adopt(left);
        }
        changed
    }
}

/// One binding's value and the objects it reaches, numbered in the order
/// the value reaches them: equal for two bindings that hold the same data,
/// wherever a session numbered their objects.
fn binding_data(value: &Value, objects: &BTreeMap<ObjectId, Object>) -> (Value, Vec<Object>) {
    let mut renamed: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    let mut reached = Vec::new();
    let mut pending = Vec::new();
    value_objects(value, &mut pending);
    while let Some(id) = pending.pop() {
        if renamed.contains_key(&id) {
            continue;
        }
        renamed.insert(id, ObjectId(reached.len() as u64));
        reached.push(id);
        if let Some(object) = objects.get(&id) {
            let mut named = Vec::new();
            object_objects(object, &mut named);
            pending.extend(named.into_iter().rev());
        }
    }
    let rename = |id: &ObjectId| renamed.get(id).copied().unwrap_or(*id);
    (
        rename_value(value, &rename),
        reached
            .iter()
            .filter_map(|id| objects.get(id))
            .map(|object| rename_object(object, &rename))
            .collect(),
    )
}

/// Refuses a heap whose bindings reach an object it does not hold, or that
/// holds an object no binding reaches: what a session writes is exactly the
/// objects its bindings reach.
fn check_reached(
    variables: &BTreeMap<Name, Value>,
    objects: &BTreeMap<ObjectId, Object>,
) -> Result<(), String> {
    let mut reached = BTreeSet::new();
    let mut pending = Vec::new();
    for value in variables.values() {
        value_objects(value, &mut pending);
    }
    while let Some(id) = pending.pop() {
        if !reached.insert(id) {
            continue;
        }
        let object = objects
            .get(&id)
            .ok_or_else(|| format!("a binding reaches object {} the state does not hold", id.0))?;
        object_objects(object, &mut pending);
    }
    match objects.keys().find(|id| !reached.contains(id)) {
        Some(id) => Err(format!("object {} is reached by no binding", id.0)),
        None => Ok(()),
    }
}

fn value_objects(value: &Value, out: &mut Vec<ObjectId>) {
    match value {
        Value::List(id)
        | Value::Map(id)
        | Value::Set(id)
        | Value::Record(id)
        | Value::Closure(id)
        | Value::Ref(Identity::Object(id)) => out.push(*id),
        Value::Tuple(items) => {
            // Reversed, so that a stack pops them in their own order.
            for item in items.iter().rev() {
                value_objects(item, out);
            }
        }
        Value::Error(error) => value_objects(&error.data, out),
        _ => {}
    }
}

/// The objects `object` names, in its own order.
fn object_objects(object: &Object, out: &mut Vec<ObjectId>) {
    let mut each = |value: &Value| {
        let mut named = Vec::new();
        value_objects(value, &mut named);
        out.extend(named.into_iter().rev());
    };
    match object {
        Object::List(items) | Object::Set(items) => items.iter().for_each(&mut each),
        Object::Map(entries) => {
            for (key, value) in entries {
                each(key);
                each(value);
            }
        }
        Object::Record(fields) => fields.iter().for_each(|(_, value)| each(value)),
        Object::Closure(closure) => out.extend(closure.captures.iter().map(|(_, cell)| *cell)),
        Object::Variable(value) => each(value),
    }
}

fn rename_value(value: &Value, rename: &dyn Fn(&ObjectId) -> ObjectId) -> Value {
    match value {
        Value::List(id) => Value::List(rename(id)),
        Value::Map(id) => Value::Map(rename(id)),
        Value::Set(id) => Value::Set(rename(id)),
        Value::Record(id) => Value::Record(rename(id)),
        Value::Closure(id) => Value::Closure(rename(id)),
        Value::Ref(Identity::Object(id)) => Value::Ref(Identity::Object(rename(id))),
        Value::Tuple(items) => Value::Tuple(
            items
                .iter()
                .map(|item| rename_value(item, rename))
                .collect(),
        ),
        Value::Error(error) => Value::Error(Arc::new(lash_kernel_doc::ErrorValue {
            kind: error.kind.clone(),
            message: error.message.clone(),
            data: rename_value(&error.data, rename),
        })),
        other => other.clone(),
    }
}

fn rename_object(object: &Object, rename: &dyn Fn(&ObjectId) -> ObjectId) -> Object {
    let value = |value: &Value| rename_value(value, rename);
    match object {
        Object::List(items) => Object::List(items.iter().map(value).collect()),
        Object::Set(items) => Object::Set(items.iter().map(value).collect()),
        Object::Map(entries) => Object::Map(
            entries
                .iter()
                .map(|(key, entry)| (value(key), value(entry)))
                .collect(),
        ),
        Object::Record(fields) => Object::Record(
            fields
                .iter()
                .map(|(name, field)| (name.clone(), value(field)))
                .collect(),
        ),
        Object::Closure(closure) => Object::Closure(lash_kernel_doc::ClosureObject {
            site: closure.site.clone(),
            captures: closure
                .captures
                .iter()
                .map(|(name, cell)| (name.clone(), rename(cell)))
                .collect(),
        }),
        Object::Variable(inner) => Object::Variable(value(inner)),
    }
}

/// One complete capture of a session's state: its SessionExecution root
/// and every node the root reaches, with their bytes.
#[derive(Clone, Debug)]
struct Capture {
    root: NodeId,
    nodes: BTreeMap<NodeId, EncodedNode>,
}

impl Capture {
    /// The capture as a graph capture: every node no acknowledged capture
    /// installed, the root included, sent with its bytes.
    fn graph(&self, installed: &BTreeSet<NodeId>) -> ExecutionGraphCapture {
        ExecutionGraphCapture::Replace {
            root: self.root,
            nodes: self
                .nodes
                .iter()
                .filter(|(id, _)| !installed.contains(id))
                .map(|(id, node)| (*id, node.clone()))
                .collect(),
        }
    }

    fn hydrated(&self) -> HydratedExecutionGraph {
        HydratedExecutionGraph {
            root: self.root,
            nodes: self.nodes.clone(),
        }
    }
}

/// The session as it stood before the active cell, with the capture
/// bookkeeping that cell may move.
struct CodeModeExecutionCheckpoint {
    bindings: SessionBindings,
    installed: BTreeSet<NodeId>,
    capture_dirty: bool,
    capture_rollback: Option<BTreeSet<NodeId>>,
    pending: Option<Capture>,
}

/// One code mode session's execution state: its bindings, the dialect its cells
/// are written in, and the bookkeeping its durable captures are built from.
pub struct CodeModeExecutionState {
    dialect: Arc<str>,
    numbers: NumberPolicy,
    bindings: SessionBindings,
    /// The nodes the last acknowledged capture's root reaches: what the
    /// store holds, whose bytes a capture need not send.
    installed: BTreeSet<NodeId>,
    /// Whether anything may have changed since the last staged capture.
    capture_dirty: bool,
    /// What `installed` was before the first capture since the last
    /// settlement: what an aborted commit goes back to.
    capture_rollback: Option<BTreeSet<NodeId>>,
    /// The staged capture, kept until its commit is acknowledged or
    /// aborted: a refused commit sends its nodes again.
    pending: Option<Capture>,
    /// What the staged capture is for.
    pending_purpose: ExecutionGraphPurpose,
    /// Whether the state as it stands is a turn's working state no session
    /// revision publishes yet: the turn's final capture owes it even when
    /// nothing changed since a working capture was acknowledged.
    final_owed: bool,
    active_execution_checkpoint: Option<CodeModeExecutionCheckpoint>,
    execution_response_returned: bool,
    /// What a saved function stored in an earlier kernel version is
    /// carried forward with.
    kernel: super::KernelCarry,
}

impl CodeModeExecutionState {
    /// The state of a session that has run no cell, whose cells are written
    /// in `dialect` and decode a host's bare numbers by `numbers`.
    pub(crate) fn new(dialect: impl Into<Arc<str>>, numbers: NumberPolicy) -> Self {
        Self {
            dialect: dialect.into(),
            numbers,
            bindings: SessionBindings::default(),
            installed: BTreeSet::new(),
            capture_dirty: true,
            capture_rollback: None,
            pending: None,
            pending_purpose: ExecutionGraphPurpose::Final,
            final_owed: false,
            active_execution_checkpoint: None,
            execution_response_returned: false,
            kernel: super::KernelCarry::default(),
        }
    }

    /// This state, carrying saved functions forward with `kernel`.
    #[must_use]
    pub(crate) fn carrying(mut self, kernel: super::KernelCarry) -> Self {
        self.kernel = kernel;
        self
    }

    /// The dialect the session recorded at its creation.
    pub(crate) fn dialect(&self) -> &str {
        &self.dialect
    }

    pub(crate) fn bindings(&self) -> &SessionBindings {
        &self.bindings
    }

    /// Every name a stored state binds, data or function: what catalog
    /// admission checks a tool's namespace against (FIG-5824).
    pub(crate) fn persisted_binding_names(
        state: &HydratedExecutionGraph,
    ) -> Result<BTreeSet<String>, CodeModeSnapshotError> {
        Ok(graph::read(state.root, &state.nodes)?
            .bindings
            .names()
            .into_iter()
            .map(|name| name.to_string())
            .collect())
    }

    /// Takes what a finished cell left (see [`SessionBindings::settle`]).
    pub(super) fn settle_cell(&mut self, cell: CellLeft<'_>) -> lash_core::BindingChanges {
        self.capture_dirty = true;
        self.bindings.settle(cell)
    }

    /// The functions the session holds, in the form another session is
    /// created with ([`Self::seed_functions`]).
    #[cfg(test)]
    pub(crate) fn saved_functions(&self) -> serde_json::Map<String, serde_json::Value> {
        bound(&self.bindings.functions)
            .into_iter()
            .filter_map(|(name, held)| {
                Some((name.to_string(), serde_json::to_value(&held.function).ok()?))
            })
            .collect()
    }

    /// Holds each of `functions` under its key, as a session is created
    /// with them. Every one is read before any is held; none is checked
    /// against what this session offers until a cell uses it.
    ///
    /// # Errors
    ///
    /// A value that is not a saved function, or a key a read-only host
    /// binding holds.
    pub async fn seed_functions(
        &mut self,
        functions: &serde_json::Map<String, serde_json::Value>,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        let mut read = Vec::with_capacity(functions.len());
        for (key, value) in functions {
            if key == HISTORY_BINDING || protected_names.contains(key) {
                return Err(SessionError::Protocol(format!(
                    "`{key}` is a read-only projected host binding; choose a different name for the saved function"
                )));
            }
            let function: SavedFunction =
                serde_json::from_value(value.clone()).map_err(|error| {
                    SessionError::Protocol(format!("`{key}` is not a saved function: {error}"))
                })?;
            if function.definition().is_none() {
                return Err(SessionError::Protocol(format!(
                    "`{key}` is not a saved function: its document does not declare `{}`",
                    function.name
                )));
            }
            read.push((Name::new(key.as_str()), function));
        }
        for (name, function) in read {
            // One stored in an earlier kernel version is carried to this
            // build's; one the migration refuses is not held, and the
            // session lists its name with why.
            match self.kernel.saved_function(&function) {
                Ok(carried) => self.bindings.hold(name, carried.unwrap_or(function)),
                Err(refusal) => self.bindings.not_migrated([name], &function, &refusal),
            }
            self.capture_dirty = true;
        }
        Ok(())
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.pending.is_some() || self.capture_dirty || self.final_owed
    }

    /// Marks the start of a cell: the session as it stands is what a
    /// cancelled or refused cell goes back to.
    pub(super) fn begin_code_execution(&mut self) {
        debug_assert!(
            !self.execution_response_returned,
            "a returned code execution must be settled before another cell starts"
        );
        self.active_execution_checkpoint = Some(CodeModeExecutionCheckpoint {
            bindings: self.bindings.clone(),
            installed: self.installed.clone(),
            capture_dirty: self.capture_dirty,
            capture_rollback: self.capture_rollback.clone(),
            pending: self.pending.clone(),
        });
        self.execution_response_returned = false;
        self.capture_dirty = true;
    }

    pub(crate) fn prepare_runtime_code_execution(&mut self) -> Result<(), &'static str> {
        if self.active_execution_checkpoint.is_none() {
            return Ok(());
        }
        if self.execution_response_returned {
            return Err("the previous code execution response has not been settled");
        }
        self.rollback_code_execution();
        Ok(())
    }

    pub(crate) fn mark_code_execution_response_returned(&mut self) {
        self.execution_response_returned = true;
    }

    pub(crate) fn accept_code_execution(&mut self) {
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
    }

    /// Goes back to the session as it stood before the active cell: its
    /// bindings, and the capture bookkeeping, so a capture the cell staged
    /// is dropped and never promoted.
    pub(crate) fn rollback_code_execution(&mut self) {
        self.execution_response_returned = false;
        let Some(checkpoint) = self.active_execution_checkpoint.take() else {
            return;
        };
        self.bindings = checkpoint.bindings;
        self.installed = checkpoint.installed;
        self.capture_dirty = checkpoint.capture_dirty;
        self.capture_rollback = checkpoint.capture_rollback;
        self.pending = checkpoint.pending;
    }

    /// Cancellation rolls the execution back; the cell's parked state is
    /// its execution's, not the session's.
    pub(crate) fn terminate_code_execution(&mut self) {
        self.rollback_code_execution();
    }

    /// Stages the complete state: the staged capture while nothing changed
    /// since it, else a new one.
    fn stage_capture(&mut self) -> Result<Capture, SessionError> {
        if !self.capture_dirty
            && let Some(pending) = &self.pending
        {
            return Ok(pending.clone());
        }
        let capture = self.build_capture()?;
        if self.capture_rollback.is_none() {
            self.capture_rollback = Some(self.installed.clone());
        }
        self.capture_dirty = false;
        self.pending = Some(capture.clone());
        Ok(capture)
    }

    /// A cell's last acknowledged park protects these nodes until the
    /// consuming phase adopts its state. Its working capture need only
    /// send nodes produced after that park, including the cell's end.
    pub(super) fn parked_nodes_stored(&mut self, nodes: BTreeSet<NodeId>) {
        self.installed.extend(nodes);
    }

    /// The session's complete SessionExecution graph (FIG-5887 §6): the
    /// root, and every node it reaches that no acknowledged capture
    /// installed. Its nodes are kept until the capture is acknowledged; a
    /// refused commit is answered by the next capture with the same
    /// nodes. A working capture and a final one write the same root.
    pub fn capture_execution_graph(
        &mut self,
        purpose: ExecutionGraphPurpose,
    ) -> Result<ExecutionGraphCapture, SessionError> {
        let capture = self.stage_capture()?;
        self.pending_purpose = purpose;
        Ok(capture.graph(&self.installed))
    }

    /// Whether the capture [`Self::capture_execution_graph`] would take
    /// now can be built, without staging it.
    pub async fn probe_execution_state_capture(&mut self) -> Result<(), SessionError> {
        if !self.capture_dirty && self.pending.is_some() {
            return Ok(());
        }
        self.build_capture().map(|_| ())
    }

    /// The complete state, with every node's body present and no capture
    /// bookkeeping touched.
    pub async fn hydrated_execution_graph(&self) -> Result<HydratedExecutionGraph, SessionError> {
        self.build_capture().map(|capture| capture.hydrated())
    }

    fn build_capture(&self) -> Result<Capture, SessionError> {
        let failed = |error: &dyn std::fmt::Display| {
            SessionError::Protocol(format!(
                "failed to encode code mode session bindings: {error}"
            ))
        };
        let mut staging = graph::staging();
        let root = graph::stage(&self.dialect, self.numbers, &self.bindings, &mut staging)
            .map_err(|error| failed(&error))?;
        let mut nodes = staging.into_nodes();
        release::complete(&mut nodes).map_err(|error| failed(&error))?;
        Ok(Capture { root, nodes })
    }

    pub(crate) fn acknowledge_execution_state_capture(&mut self) {
        self.capture_rollback = None;
        if let Some(pending) = self.pending.take() {
            self.installed = pending.nodes.into_keys().collect();
            // A working capture's commit roots the state under its turn; only
            // a final one publishes it as the session's.
            self.final_owed = self.pending_purpose == ExecutionGraphPurpose::Working;
        }
    }

    pub(crate) fn abort_execution_state_capture(&mut self) {
        let Some(rollback) = self.capture_rollback.take() else {
            return;
        };
        // The installed set is the rollback's, so the next capture sends
        // every node the store may not hold.
        self.installed = rollback;
        self.capture_dirty = true;
        self.pending = None;
    }

    /// Installs a hydrated SessionExecution graph (FIG-5887 §6): the
    /// working state a turn phase owns, or a session revision's, as
    /// `purpose` says, before any model continuation or cell compiles. A
    /// working state is one the turn's final capture still owes.
    pub fn restore_execution_graph(
        &mut self,
        graph: &HydratedExecutionGraph,
        purpose: ExecutionGraphPurpose,
    ) -> Result<(), CodeModeSnapshotError> {
        self.restore(graph.root, &graph.nodes)?;
        self.final_owed = purpose == ExecutionGraphPurpose::Working;
        Ok(())
    }

    /// Restores the state rooted at `root`. A state recorded under another
    /// dialect or number policy is refused: a session's are the ones it
    /// recorded at its creation. Nothing is adopted when any check fails.
    fn restore(
        &mut self,
        root: NodeId,
        nodes: &BTreeMap<NodeId, EncodedNode>,
    ) -> Result<(), CodeModeSnapshotError> {
        let read = graph::read(root, nodes)?;
        if read.dialect != self.dialect.as_ref() {
            return Err(CodeModeSnapshotError::DialectMismatch {
                expected: self.dialect.to_string(),
                found: read.dialect,
            });
        }
        if read.numbers != self.numbers {
            return Err(CodeModeSnapshotError::NumberPolicyMismatch {
                expected: self.numbers,
                found: read.numbers,
            });
        }
        // A state an earlier kernel version wrote is rewritten by the next
        // capture in this build's: what an idle session's carry recaptures
        // (FIG-5787).
        let stale_kernel = read.kernel != KERNEL_VERSION;
        let mut bindings = read.bindings;
        // A saved function stored in an earlier kernel version is carried
        // to this build's, each one alone; the next capture stores what was
        // carried. One the migration refuses is not held: the session lists
        // its name with why, as it does a binding that was not saved.
        let mut carried_functions = false;
        let mut not_migrated = Vec::new();
        for (name, held) in &mut bindings.functions {
            match self.kernel.saved_function(&held.function) {
                Ok(None) => {}
                Ok(Some(carried)) => {
                    held.function = carried;
                    carried_functions = true;
                }
                Err(refusal) => {
                    let names = std::iter::once(name.clone()).chain(held.aliases.iter().cloned());
                    not_migrated.push((names.collect::<Vec<_>>(), held.function.clone(), refusal));
                    carried_functions = true;
                }
            }
        }
        for (names, function, refusal) in not_migrated {
            bindings.not_migrated(names, &function, &refusal);
        }
        // The history binding is the projection's; a stored state never
        // shadows it.
        let pruned_reserved = bindings.remove(&BTreeSet::from([HISTORY_BINDING.to_string()]));
        self.bindings = bindings;
        self.installed = nodes.keys().copied().collect();
        self.capture_dirty = pruned_reserved || carried_functions || stale_kernel;
        self.capture_rollback = None;
        self.pending = None;
        self.active_execution_checkpoint = None;
        self.execution_response_returned = false;
        Ok(())
    }

    /// Drops every binding a read-only projected name now shadows.
    pub async fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        let mut names = protected_names.clone();
        names.insert(HISTORY_BINDING.to_string());
        if self.bindings.remove(&names) {
            self.capture_dirty = true;
        }
        Ok(())
    }

    /// Applies a `set_default` patch as one transaction: every key is
    /// checked before any is bound, and a name the session already binds
    /// keeps its value.
    pub async fn patch_globals(
        &mut self,
        patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        for key in patch.set_default.keys() {
            if key == HISTORY_BINDING || protected_names.contains(key) {
                return Err(SessionError::Protocol(format!(
                    "`{key}` is a read-only projected host binding; choose a different variable name for `set_default`"
                )));
            }
        }
        for (key, value) in &patch.set_default {
            if self
                .bindings
                .variables
                .contains_key(&Name::new(key.as_str()))
            {
                continue;
            }
            self.bindings.seed(key, value, self.numbers);
            self.capture_dirty = true;
        }
        Ok(())
    }

    /// The complete graph of the state as it stands, every node present.
    #[cfg(test)]
    pub(crate) fn hydrated_graph(&self) -> HydratedExecutionGraph {
        let capture = self.build_capture().expect("the state encodes");
        HydratedExecutionGraph {
            root: capture.root,
            nodes: capture.nodes,
        }
    }

    /// The bundle each binding that holds a saved function names in
    /// `graph`.
    #[cfg(test)]
    pub(crate) fn bundles(graph: &HydratedExecutionGraph) -> BTreeMap<String, NodeId> {
        use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder, NodeKind};
        let body: graph::SessionExecutionBody =
            GraphDecoder::new(&graph.nodes, DecodeLimits::within_total(u64::MAX))
                .decode(graph.root, NodeKind::SESSION_EXECUTION)
                .expect("a SessionExecution root");
        body.aliases
            .into_iter()
            .map(|row| (row.binding.to_string(), NodeId::from(row.bundle)))
            .collect()
    }

    /// Every binding the session holds.
    #[cfg(test)]
    pub(crate) fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.bindings.variables.keys().map(Name::as_str)
    }

    /// The session's bindings as JSON for the "Bound Variables" prompt
    /// section, without the reserved history binding and the `exclude`
    /// names, which are read-only values with a section of their own.
    /// The session's bound variables but `exclude`: a host resource handle
    /// as itself, which stays opaque, and every other value as the host
    /// reads it.
    pub(crate) fn bound_variable_values(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Vec<(String, crate::session_globals::SessionGlobalKind)> {
        self.bindings
            .variables
            .iter()
            .filter(|(name, _)| {
                name.as_str() != HISTORY_BINDING && !exclude.contains(name.as_str())
            })
            .map(|(name, value)| {
                let kind = match value {
                    Value::Handle(handle) => crate::session_globals::SessionGlobalKind::Handle {
                        kind: handle.kind.clone(),
                        id: handle.id.clone(),
                    },
                    value => crate::session_globals::SessionGlobalKind::Data(
                        crate::cell_value::binding_json(value, &self.bindings.objects),
                    ),
                };
                (name.to_string(), kind)
            })
            .collect()
    }
}

/// The library functions each function a session saved pins, by the
/// name it is declared under, read from `state`, the stored execution state
/// of the session (FIG-5799): what a build that drops a helper release asks
/// of an idle session.
///
/// # Errors
///
/// The refusal of a state that is not one this build reads.
pub fn saved_function_pins(
    state: &HydratedExecutionGraph,
) -> Result<BTreeMap<String, BTreeSet<lash_kernel_doc::FunctionId>>, CodeModeSnapshotError> {
    Ok(graph::read(state.root, &state.nodes)?
        .bindings
        .functions
        .into_iter()
        .map(|(name, held)| {
            (
                name.to_string(),
                held.function
                    .document
                    .manifest
                    .functions
                    .keys()
                    .copied()
                    .collect(),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests;
