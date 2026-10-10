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
//! session holds those beside its bindings, a later cell that names one
//! has it declared in its own document, and in that cell the binding is a
//! reference to the declaration. Any other such binding is dropped, and
//! the session remembers its name and the reason, so a later cell that
//! reads it is told why it is gone.
//!
//! The state is saved in the kernel's parked-run terms
//! (`lash-kernel-state`): a header and one fragment per session binding,
//! each fragment holding its binding's value and the heap objects it owns.
//! A capture rewrites only the fragments whose bytes changed. For that to
//! hold from cell to cell, a binding's objects keep their identities while
//! the binding keeps its shape: the machine numbers a run's objects afresh,
//! so the session renumbers what a cell leaves into each binding's own
//! range ([`SessionBindings::settle`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lash_core::plugin::{
    EXECUTION_STATE_LEAF_MIN_BODY_BYTES, ExecutionLeafName, ExecutionStateCapture,
    HydratedExecutionState, LeafChange,
};
use lash_kernel_dialect::{NotSaved, SavedFunction};
use lash_kernel_doc::{
    Annotations, Document, DocumentId, Identity, KERNEL_VERSION, Name, NumberPolicy, Object,
    ObjectId, Value,
};
use lash_kernel_state::{Baseline, ParkedRun, Root, Run, SavedFragment};
use lash_kernel_vm::Bindings;
use serde::{Deserialize, Serialize};

use super::snapshot::{RLM_SNAPSHOT_VERSION, RlmSnapshotError};

/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION, leaf_component_key))
const LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION: &str = "lash-rlm-execution-state-leaf/v2";

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
    /// The functions the session holds, by the binding each is called
    /// through.
    functions: BTreeMap<Name, HeldFunction>,
    /// How many cells have left this session their bindings.
    cells: u64,
    /// The document of the cell that last left these bindings.
    document: Option<DocumentId>,
}

/// Whether `dialect`'s front end declares the saved functions a cell names
/// (`lash_kernel_dialect::install`).
fn declares_saved_functions(dialect: &str) -> bool {
    matches!(dialect, "typescript" | "python")
}

/// A saved function as a session holds it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeldFunction {
    pub(crate) function: SavedFunction,
    /// The session's cell, counted from one, whose end froze the
    /// function's captures. `None` for a function the session was created
    /// with.
    pub(crate) cell: Option<u64>,
}

/// What a finished cell left a session.
pub(super) struct CellLeft<'a> {
    pub document: &'a Document,
    pub identity: DocumentId,
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
}

impl SessionBindings {
    /// The bindings a cell's machine starts with.
    pub(crate) fn start(&self) -> Bindings {
        let mut variables = self.variables.clone();
        // A saved function is, in a cell, a reference to its declaration
        // in that cell's document.
        for name in self.functions.keys() {
            variables.insert(name.clone(), Value::Function(name.clone()));
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
            .chain(self.functions.keys())
            .cloned()
            .collect()
    }

    /// The functions a cell is lowered against.
    pub(crate) fn functions(&self) -> BTreeMap<Name, SavedFunction> {
        self.functions
            .iter()
            .map(|(name, held)| (name.clone(), held.function.clone()))
            .collect()
    }

    pub(crate) fn held_functions(&self) -> &BTreeMap<Name, HeldFunction> {
        &self.functions
    }

    pub(crate) fn not_carried(&self) -> &BTreeMap<Name, NotSaved> {
        &self.not_carried
    }

    /// Takes what a cell left. A binding that holds a function whose
    /// captures are data or other saved functions becomes a saved
    /// function; a binding still bound to the reference it started as
    /// keeps the function it names. Where the session's dialect declares
    /// no saved function (`declares_saved`), none is saved. Answers what
    /// that did to the bindings, every name: a saved function is a binding
    /// like any other.
    pub(super) fn settle(
        &mut self,
        cell: CellLeft<'_>,
        declares_saved: bool,
    ) -> lash_core::BindingChanges {
        self.document = Some(cell.identity);
        self.cells += 1;
        let held: BTreeSet<Name> = self.functions.keys().cloned().collect();
        let (saved, refused) = if declares_saved {
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
        let mut functions = BTreeMap::new();
        left.variables.retain(|name, value| {
            let Value::Function(target) = &*value else {
                return true;
            };
            // A reference to a saved function is that function, under
            // whichever name holds it now.
            let Some(held) = self.functions.get(target) else {
                return true;
            };
            let function = if name == target {
                held.function.clone()
            } else {
                held.function.renamed(name)
            };
            functions.insert(
                name.clone(),
                HeldFunction {
                    function,
                    cell: held.cell,
                },
            );
            false
        });
        for (name, function) in saved {
            functions.insert(
                name,
                HeldFunction {
                    function,
                    cell: Some(self.cells),
                },
            );
        }
        self.not_carried
            .retain(|name, _| !left.variables.contains_key(name) && !functions.contains_key(name));
        let before = Bindings {
            variables: std::mem::take(&mut self.variables),
            objects: std::mem::take(&mut self.objects),
        };
        let before_functions = std::mem::replace(&mut self.functions, functions);
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
        for (name, held) in &self.functions {
            match before_functions.get(name) {
                Some(was) if was.function == held.function => {}
                None if !before.variables.contains_key(name) => {
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
            .chain(before_functions.keys())
            .filter(|name| {
                !self.variables.contains_key(*name)
                    && !self.functions.contains_key(*name)
                    && !refused.contains_key(*name)
            })
            .map(ToString::to_string)
            .collect();
        self.not_carried.extend(refused);
        changes
    }

    /// Records that `function`, stored under `name` in an earlier kernel
    /// version, was refused by the migration to this build's: the session
    /// holds no function under the name, and lists it with why.
    fn not_migrated(
        &mut self,
        name: Name,
        function: &SavedFunction,
        refusal: &lash_vm_runtime::KernelMigrationRefusal,
    ) {
        self.functions.remove(&name);
        self.not_carried.insert(
            name,
            NotSaved::NotMigrated {
                from: function.document.manifest.kernel,
                problem: refusal.to_string(),
            },
        );
    }

    /// Holds `function` under `name`, as a session is created with it.
    fn hold(&mut self, name: Name, function: SavedFunction) {
        let function = if function.name == name {
            function
        } else {
            function.renamed(&name)
        };
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
        self.functions.remove(&Name::new(name));
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
        let before = left.variables.len() + self.not_carried.len() + self.functions.len();
        left.variables
            .retain(|name, _| !names.contains(name.as_str()));
        self.not_carried
            .retain(|name, _| !names.contains(name.as_str()));
        self.functions
            .retain(|name, _| !names.contains(name.as_str()));
        let changed =
            left.variables.len() + self.not_carried.len() + self.functions.len() != before;
        if changed {
            self.adopt(left);
        }
        changed
    }

    /// The bindings as a parked run that has no task: what
    /// `lash-kernel-state` writes one fragment per binding of.
    fn parked(&self) -> ParkedRun {
        ParkedRun {
            run: Run {
                kernel: KERNEL_VERSION,
                document: self
                    .document
                    .unwrap_or_else(|| DocumentId::from_bytes([0; 32])),
                functions: BTreeSet::new(),
                charged: 0,
                objects_allocated: 0,
                waits_issued: 0,
                ready: Vec::new(),
                withdrawn: BTreeSet::new(),
                unreported: Vec::new(),
            },
            session: self.variables.clone(),
            tasks: Vec::new(),
            objects: self.objects.clone(),
        }
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

/// The stored root of a session's execution state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RlmSnapshotRoot {
    version: u32,
    /// The dialect the session's cells are written in.
    dialect: String,
    /// The parked-run header: the kernel version and the session's roots.
    header: String,
    /// One stored fragment per session binding.
    bindings: BTreeMap<String, PersistedValue>,
    /// The slot each binding's objects are numbered in.
    slots: BTreeMap<String, u32>,
    next_slot: u32,
    /// The bindings a cell left that were not carried (`K-SES-003`), each
    /// with why.
    not_carried: BTreeMap<String, NotSaved>,
    /// The functions the session holds, by binding
    /// (`lash_vm_runtime::KERNEL_SAVED_FUNCTION_VERSION`).
    functions: BTreeMap<String, HeldFunction>,
    /// How many cells have left the session their bindings.
    cells: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedValue {
    Inline { body: String },
    Leaf { component: ExecutionLeafName },
}

fn leaf_component_key(body: &[u8]) -> ExecutionLeafName {
    ExecutionLeafName::new(format!(
        "blake3/{}",
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_RLM_EXECUTION_STATE_LEAF_DOMAIN_VERSION,
            body,
        )
    ))
}

fn root_leaf_keys(bindings: &BTreeMap<String, PersistedValue>) -> BTreeSet<ExecutionLeafName> {
    bindings
        .values()
        .filter_map(|value| match value {
            PersistedValue::Leaf { component } => Some(component.clone()),
            PersistedValue::Inline { .. } => None,
        })
        .collect()
}

/// Which state a capture is relative to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// A checkpoint delta: every leaf the receiver already holds is
    /// referenced, not sent.
    Incremental,
    /// Every binding, with every leaf body present.
    Complete,
}

struct PreparedCapture {
    snapshot: ExecutionStateCapture,
    leaf_keys: BTreeSet<ExecutionLeafName>,
    #[cfg(test)]
    written_leaves: usize,
}

/// The session as it stood before the active cell, with the capture
/// bookkeeping that cell may move.
struct RlmExecutionCheckpoint {
    bindings: SessionBindings,
    persisted_leaf_keys: BTreeSet<ExecutionLeafName>,
    capture_dirty: bool,
    capture_rollback: Option<BTreeSet<ExecutionLeafName>>,
    pending_snapshot: Option<ExecutionStateCapture>,
}

/// One RLM session's execution state: its bindings, the dialect its cells
/// are written in, and the bookkeeping its durable captures are built from.
pub struct RlmExecutionState {
    dialect: Arc<str>,
    numbers: NumberPolicy,
    bindings: SessionBindings,
    /// The leaves the store holds for the last installed capture.
    persisted_leaf_keys: BTreeSet<ExecutionLeafName>,
    /// Whether anything may have changed since the last installed capture.
    capture_dirty: bool,
    /// The leaves the store held before the first capture since the last
    /// settlement: what an aborted commit goes back to.
    capture_rollback: Option<BTreeSet<ExecutionLeafName>>,
    pending_snapshot: Option<ExecutionStateCapture>,
    active_execution_checkpoint: Option<RlmExecutionCheckpoint>,
    execution_response_returned: bool,
    /// What a saved function stored in an earlier kernel version is
    /// carried forward with.
    kernel: super::KernelCarry,
    #[cfg(test)]
    written_leaves_in_last_snapshot: usize,
}

impl RlmExecutionState {
    /// The state of a session that has run no cell, whose cells are written
    /// in `dialect` and decode a host's bare numbers by `numbers`.
    pub(crate) fn new(dialect: impl Into<Arc<str>>, numbers: NumberPolicy) -> Self {
        Self {
            dialect: dialect.into(),
            numbers,
            bindings: SessionBindings::default(),
            persisted_leaf_keys: BTreeSet::new(),
            capture_dirty: true,
            capture_rollback: None,
            pending_snapshot: None,
            active_execution_checkpoint: None,
            execution_response_returned: false,
            kernel: super::KernelCarry::default(),
            #[cfg(test)]
            written_leaves_in_last_snapshot: 0,
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

    /// Takes what a finished cell left (see [`SessionBindings::settle`]).
    pub(super) fn settle_cell(&mut self, cell: CellLeft<'_>) -> lash_core::BindingChanges {
        self.capture_dirty = true;
        self.bindings
            .settle(cell, declares_saved_functions(&self.dialect))
    }

    /// The functions the session holds, in the form another session is
    /// created with ([`Self::seed_functions`]).
    #[cfg(test)]
    pub(crate) fn saved_functions(&self) -> serde_json::Map<String, serde_json::Value> {
        self.bindings
            .functions
            .iter()
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
                Err(refusal) => self.bindings.not_migrated(name, &function, &refusal),
            }
            self.capture_dirty = true;
        }
        Ok(())
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.pending_snapshot.is_some() || self.capture_dirty
    }

    /// Marks the start of a cell: the session as it stands is what a
    /// cancelled or refused cell goes back to.
    pub(super) fn begin_code_execution(&mut self) {
        debug_assert!(
            !self.execution_response_returned,
            "a returned code execution must be settled before another cell starts"
        );
        self.active_execution_checkpoint = Some(RlmExecutionCheckpoint {
            bindings: self.bindings.clone(),
            persisted_leaf_keys: self.persisted_leaf_keys.clone(),
            capture_dirty: self.capture_dirty,
            capture_rollback: self.capture_rollback.clone(),
            pending_snapshot: self.pending_snapshot.clone(),
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

    pub(crate) fn rollback_code_execution(&mut self) {
        self.execution_response_returned = false;
        let Some(checkpoint) = self.active_execution_checkpoint.take() else {
            return;
        };
        self.bindings = checkpoint.bindings;
        self.persisted_leaf_keys = checkpoint.persisted_leaf_keys;
        self.capture_dirty = checkpoint.capture_dirty;
        self.capture_rollback = checkpoint.capture_rollback;
        self.pending_snapshot = checkpoint.pending_snapshot;
    }

    /// Cancellation rolls the execution back; the cell's parked state is
    /// its execution's, not the session's.
    pub(crate) fn terminate_code_execution(&mut self) {
        self.rollback_code_execution();
    }

    /// The root and only the fragments whose bytes the store does not
    /// already hold.
    ///
    /// `fleet_format` is the `F` the bound session's store recorded: the
    /// root stamps `F`'s writer version, never the bare build constant.
    pub async fn snapshot_execution_state(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<ExecutionStateCapture, SessionError> {
        if !self.capture_dirty
            && let Some(snapshot) = &self.pending_snapshot
        {
            return Ok(snapshot.clone());
        }
        let prepared = self.build_capture(CaptureMode::Incremental, fleet_format)?;
        let rollback = std::mem::replace(&mut self.persisted_leaf_keys, prepared.leaf_keys);
        if self.capture_rollback.is_none() {
            self.capture_rollback = Some(rollback);
        }
        self.capture_dirty = false;
        #[cfg(test)]
        {
            self.written_leaves_in_last_snapshot = prepared.written_leaves;
        }
        self.pending_snapshot = Some(prepared.snapshot.clone());
        Ok(prepared.snapshot)
    }

    /// Whether the capture [`Self::snapshot_execution_state`] would take
    /// now can be built, without staging it.
    pub async fn probe_execution_state_capture(
        &mut self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), SessionError> {
        if !self.capture_dirty && self.pending_snapshot.is_some() {
            return Ok(());
        }
        self.build_capture(CaptureMode::Incremental, fleet_format)
            .map(|_| ())
    }

    /// The complete state, with every leaf body present and no capture
    /// bookkeeping touched.
    pub async fn hydrated_execution_state(
        &self,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<HydratedExecutionState, SessionError> {
        let prepared = self.build_capture(CaptureMode::Complete, fleet_format)?;
        let ExecutionStateCapture::Replace { root, leaves } = prepared.snapshot else {
            return Err(SessionError::Protocol(
                "RLM root was not encoded".to_string(),
            ));
        };
        let mut components = BTreeMap::new();
        for (key, component) in leaves {
            match component {
                LeafChange::Changed(body) => {
                    components.insert(key, body);
                }
                LeafChange::Unchanged => {
                    return Err(SessionError::Protocol(format!(
                        "complete RLM execution state referenced leaf `{key:?}` without its body"
                    )));
                }
            }
        }
        Ok(HydratedExecutionState { root, components })
    }

    fn build_capture(
        &self,
        mode: CaptureMode,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<PreparedCapture, SessionError> {
        let encode = |error: &dyn std::fmt::Display| {
            SessionError::Protocol(format!("failed to encode RLM session bindings: {error}"))
        };
        // Every fragment is written, and compared with what the store holds
        // by its content address: a binding's bytes are a function of the
        // binding alone (see the module docs).
        let saved = self
            .bindings
            .parked()
            .save(&Baseline::default())
            .map_err(|error| encode(&error))?;
        // Leaves the receiver of this capture already holds. A staged but
        // uncommitted capture supersedes the durable set.
        let held: BTreeSet<ExecutionLeafName> = match (mode, &self.pending_snapshot) {
            (CaptureMode::Complete, _) => BTreeSet::new(),
            (CaptureMode::Incremental, Some(pending)) => pending.leaves().keys().cloned().collect(),
            (CaptureMode::Incremental, None) => self.persisted_leaf_keys.clone(),
        };
        // A leaf a staged capture carries is not durable yet: it is sent
        // again.
        let mut staged: BTreeMap<ExecutionLeafName, Arc<[u8]>> =
            match (mode, &self.pending_snapshot) {
                (CaptureMode::Incremental, Some(pending)) => pending
                    .leaves()
                    .iter()
                    .filter_map(|(key, change)| match change {
                        LeafChange::Changed(body) => Some((key.clone(), Arc::clone(body))),
                        LeafChange::Unchanged => None,
                    })
                    .collect(),
                _ => BTreeMap::new(),
            };
        let mut bindings = BTreeMap::new();
        let mut leaves = BTreeMap::new();
        #[cfg(test)]
        let mut written_leaves = 0;
        for (root, fragment) in saved.fragments {
            let (Root::Session(name), SavedFragment::Changed(body)) = (root, fragment) else {
                return Err(SessionError::Protocol(
                    "a session's state has a root that is no session binding".to_string(),
                ));
            };
            let persisted = if body.len() >= EXECUTION_STATE_LEAF_MIN_BODY_BYTES {
                let component = leaf_component_key(&body);
                let change = match staged.remove(&component) {
                    Some(body) => LeafChange::Changed(body),
                    None if held.contains(&component) => LeafChange::Unchanged,
                    None => LeafChange::Changed(body.into()),
                };
                #[cfg(test)]
                if matches!(change, LeafChange::Changed(_)) {
                    written_leaves += 1;
                }
                leaves.insert(component.clone(), change);
                PersistedValue::Leaf { component }
            } else {
                PersistedValue::Inline {
                    body: String::from_utf8(body).map_err(|error| encode(&error))?,
                }
            };
            bindings.insert(name.to_string(), persisted);
        }
        let root = RlmSnapshotRoot {
            version: fleet_format.writer_version(lash_core::surface_format!(RLM_SNAPSHOT_VERSION)),
            dialect: self.dialect.to_string(),
            header: String::from_utf8(saved.header).map_err(|error| encode(&error))?,
            bindings,
            slots: self
                .bindings
                .slots
                .iter()
                .map(|(name, slot)| (name.to_string(), *slot))
                .collect(),
            next_slot: self.bindings.next_slot,
            not_carried: self
                .bindings
                .not_carried
                .iter()
                .map(|(name, why)| (name.to_string(), why.clone()))
                .collect(),
            functions: self
                .bindings
                .functions
                .iter()
                .map(|(name, held)| (name.to_string(), held.clone()))
                .collect(),
            cells: self.bindings.cells,
        };
        let leaf_keys = root_leaf_keys(&root.bindings);
        let encoded = serde_json::to_vec(&root).map_err(|error| encode(&error))?;
        Ok(PreparedCapture {
            snapshot: ExecutionStateCapture::Replace {
                root: encoded.into(),
                leaves,
            },
            leaf_keys,
            #[cfg(test)]
            written_leaves,
        })
    }

    pub(crate) fn acknowledge_execution_state_capture(&mut self) {
        self.capture_rollback = None;
        self.pending_snapshot = None;
    }

    pub(crate) fn abort_execution_state_capture(&mut self) {
        let Some(rollback) = self.capture_rollback.take() else {
            return;
        };
        // The durable set is the rollback's, so the next capture compares
        // against what the store holds.
        self.persisted_leaf_keys = rollback;
        self.capture_dirty = true;
        self.pending_snapshot = None;
    }

    /// Restores a stored execution state.
    ///
    /// `fleet_format` is the `F` the bound store recorded: the read admits
    /// every version of the snapshot surface's read window. A state recorded
    /// under another dialect is refused: a session's dialect is the one it
    /// recorded at its creation.
    pub async fn restore_execution_state(
        &mut self,
        state: &HydratedExecutionState,
        fleet_format: lash_core::FleetFormat,
    ) -> Result<(), RlmSnapshotError> {
        let format = |details: String| RlmSnapshotError::FormatMismatch { details };
        let window = fleet_format.read_window(lash_core::surface_format!(RLM_SNAPSHOT_VERSION));
        let parsed: RlmSnapshotRoot =
            serde_json::from_slice(&state.root).map_err(|error| format(error.to_string()))?;
        if !window.admits(parsed.version) {
            return Err(RlmSnapshotError::VersionMismatch {
                expected: window.newest(),
                found: parsed.version,
            });
        }
        if parsed.dialect != self.dialect.as_ref() {
            return Err(RlmSnapshotError::DialectMismatch {
                expected: self.dialect.to_string(),
                found: parsed.dialect,
            });
        }
        let expected_leaf_keys = root_leaf_keys(&parsed.bindings);
        let supplied_leaf_keys = state.components.keys().cloned().collect::<BTreeSet<_>>();
        if expected_leaf_keys != supplied_leaf_keys {
            return Err(RlmSnapshotError::LeafSetMismatch {
                missing: expected_leaf_keys
                    .difference(&supplied_leaf_keys)
                    .cloned()
                    .collect(),
                unexpected: supplied_leaf_keys
                    .difference(&expected_leaf_keys)
                    .cloned()
                    .collect(),
            });
        }
        let mut fragments: Vec<(Root, &[u8])> = Vec::with_capacity(parsed.bindings.len());
        for (name, persisted) in &parsed.bindings {
            let body: &[u8] = match persisted {
                PersistedValue::Inline { body } => body.as_bytes(),
                PersistedValue::Leaf { component } => {
                    let body = state.components.get(component).ok_or_else(|| {
                        RlmSnapshotError::MissingLeaf {
                            logical_key: name.clone(),
                            component: component.clone(),
                        }
                    })?;
                    let actual_component = leaf_component_key(body);
                    if &actual_component != component {
                        return Err(RlmSnapshotError::LeafHashMismatch {
                            logical_key: name.clone(),
                            component: component.clone(),
                            actual_component,
                        });
                    }
                    body
                }
            };
            fragments.push((Root::Session(Name::new(name.as_str())), body));
        }
        let (parked, _) = ParkedRun::load(
            parsed.header.as_bytes(),
            fragments.iter().map(|(root, body)| (root, *body)),
        )
        .map_err(RlmSnapshotError::Kernel)?;
        if !parked.tasks.is_empty() {
            return Err(format(
                "a session's stored state holds a task; only bindings are a session's".to_string(),
            ));
        }
        // A header an earlier kernel version wrote is rewritten by the next
        // capture in this build's: what an idle session's carry recaptures
        // (FIG-5787).
        let stale_header = parked.run.kernel != KERNEL_VERSION;
        // A saved function stored in an earlier kernel version is carried
        // to this build's, each one alone; the next capture stores what was
        // carried. One the migration refuses is not held: the session lists
        // its name with why, as it does a binding that was not saved.
        let mut carried_functions = false;
        let mut functions = BTreeMap::new();
        let mut not_migrated = Vec::new();
        for (name, mut held) in parsed.functions {
            match self.kernel.saved_function(&held.function) {
                Ok(None) => {}
                Ok(Some(carried)) => {
                    held.function = carried;
                    carried_functions = true;
                }
                Err(refusal) => {
                    not_migrated.push((Name::new(name), held.function, refusal));
                    carried_functions = true;
                    continue;
                }
            }
            functions.insert(Name::new(name), held);
        }
        let mut bindings = SessionBindings {
            variables: parked.session,
            objects: parked.objects,
            slots: parsed
                .slots
                .into_iter()
                .map(|(name, slot)| (Name::new(name), slot))
                .collect(),
            next_slot: parsed.next_slot,
            not_carried: parsed
                .not_carried
                .into_iter()
                .map(|(name, why)| (Name::new(name), why))
                .collect(),
            functions,
            cells: parsed.cells,
            document: Some(parked.run.document),
        };
        for (name, function, refusal) in not_migrated {
            bindings.not_migrated(name, &function, &refusal);
        }
        // The history binding is the projection's; a stored state never
        // shadows it.
        let pruned_reserved = bindings.remove(&BTreeSet::from([HISTORY_BINDING.to_string()]));
        self.bindings = bindings;
        self.persisted_leaf_keys = expected_leaf_keys;
        self.capture_dirty = pruned_reserved || carried_functions || stale_header;
        self.capture_rollback = None;
        self.pending_snapshot = None;
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

    /// Every binding the session holds.
    #[cfg(test)]
    pub(crate) fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.bindings.variables.keys().map(Name::as_str)
    }

    /// How many leaves the last capture sent a body for.
    #[cfg(test)]
    pub(super) fn written_leaves_in_last_snapshot(&self) -> usize {
        self.written_leaves_in_last_snapshot
    }

    /// The session's bindings as JSON for the "Bound Variables" prompt
    /// section, without the reserved history binding and the `exclude`
    /// names, which are read-only values with a section of their own.
    pub(crate) fn bound_variable_values(
        &self,
        exclude: &BTreeSet<String>,
    ) -> Vec<(String, serde_json::Value)> {
        self.bindings
            .variables
            .iter()
            .filter(|(name, _)| {
                name.as_str() != HISTORY_BINDING && !exclude.contains(name.as_str())
            })
            .map(|(name, value)| {
                (
                    name.to_string(),
                    crate::cell_value::binding_json(value, &self.bindings.objects),
                )
            })
            .collect()
    }
}

/// The library functions each function a session saved pins, by the
/// binding it is called through, read from `root`, the stored root of the
/// session's execution state (FIG-5799): what a build that drops a helper
/// release asks of an idle session.
///
/// # Errors
///
/// [`RlmSnapshotError::FormatMismatch`] when `root` is not one.
pub fn saved_function_pins(
    root: &[u8],
) -> Result<BTreeMap<String, BTreeSet<lash_kernel_doc::FunctionId>>, RlmSnapshotError> {
    let parsed: RlmSnapshotRoot =
        serde_json::from_slice(root).map_err(|error| RlmSnapshotError::FormatMismatch {
            details: error.to_string(),
        })?;
    Ok(parsed
        .functions
        .into_iter()
        .map(|(name, held)| {
            (
                name,
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
