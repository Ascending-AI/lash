//! How a run starts and what it leaves: the entry's arguments and the
//! session's bindings (`K-SES-001` to `K-SES-003`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorValue, Identity, Object, ObjectId, TaskId, TaskIdentity, Value, validate_document,
};

use super::{Interrupt, KernelMachine, Task, TaskState};
use crate::Layout;
use crate::compile::compile;
use crate::heap::{Heap, Key, Obj, Table, object_bytes, value_bytes};
use crate::interface::{Bindings, Bounds, Finished, Program, Start, StartError, Target};

pub(super) fn start(
    program: Program,
    bounds: Bounds,
    start: Start,
    layout: Layout,
) -> Result<KernelMachine, StartError> {
    for function in program.document.manifest.functions.keys() {
        if program.registry.get(function).is_none() {
            return Err(StartError::MissingFunction {
                function: *function,
            });
        }
    }
    validate_document(&program.document, program.registry.as_ref())?;
    let in_flight = BTreeSet::new();
    let exe =
        compile(&program.document, &program.registry, layout, &in_flight).map_err(|missing| {
            StartError::MissingFunction {
                function: missing.0,
            }
        })?;
    let exe = Arc::new(exe);
    let mut machine = KernelMachine {
        heap: Heap::new(bounds.memory),
        document: None,
        exe: Arc::clone(&exe),
        bounds,
        tasks: vec![Task {
            identity: TaskIdentity::Main,
            frames: Vec::new(),
            state: TaskState::Ready,
            incoming: None,
            joiners: Vec::new(),
            observed: false,
            passed: false,
            failed: false,
            occurrences: BTreeMap::new(),
        }],
        ready: VecDeque::new(),
        current: Some(TaskId::MAIN),
        joins: BTreeMap::new(),
        next_join: 0,
        waits: BTreeMap::new(),
        next_wait: 0,
        requests: Vec::new(),
        withdrawn: Vec::new(),
        withdrawn_ever: BTreeSet::new(),
        session: BTreeMap::new(),
        session_cell: matches!(start.target, Target::Main),
        pins: Vec::new(),
        fresh: Vec::new(),
        charging: true,
        charged: 0,
        live_tasks: 1,
        inline_depth: 0,
        inline_result: None,
        end: None,
        ended: false,
        program,
    };
    let arguments = |problem: String| StartError::Arguments { problem };
    let (code, args) = match &start.target {
        Target::Main => {
            if !start.args.is_empty() {
                return Err(arguments("`main` takes no arguments".to_string()));
            }
            machine.load(&start.bindings)?;
            (exe.main, Vec::new())
        }
        Target::Entry(entry) => {
            let document = Arc::clone(&machine.program.document);
            let (Some(signature), Some(code)) =
                (document.entries.get(entry), exe.declared.get(entry))
            else {
                return Err(StartError::UnknownEntry {
                    entry: entry.clone(),
                });
            };
            if !start.bindings.variables.is_empty() || !start.bindings.objects.is_empty() {
                return Err(StartError::Bindings {
                    problem: "an entry starts with no session bindings".to_string(),
                });
            }
            let given = start.args.len();
            if !(signature.required()..=signature.params.len()).contains(&given) {
                return Err(arguments(format!(
                    "`{entry}` takes {} to {} argument(s); {given} given",
                    signature.required(),
                    signature.params.len()
                )));
            }
            let mut args = Vec::with_capacity(given);
            for (datum, param) in start.args.iter().zip(&signature.params) {
                let value = machine
                    .decode(&exe, datum, &param.ty)
                    .map_err(|interrupt| arguments(describe(param.name.as_str(), interrupt)))?;
                args.push(value);
            }
            // Decoding an argument is the embedder's act, not the run's.
            machine.charged = 0;
            (*code, args)
        }
    };
    machine
        .push_frame(TaskId::MAIN, &exe, super::exec::Call::new(code, args))
        .map_err(|interrupt| arguments(describe("the start", interrupt)))?;
    Ok(machine)
}

fn describe(what: &str, interrupt: Interrupt) -> String {
    match interrupt {
        Interrupt::Raise(Value::Error(error)) => format!("{what}: {}", error.message),
        Interrupt::Raise(_) => format!("{what}: refused"),
        Interrupt::Halt(halt) => format!("{what}: {halt:?}"),
    }
}

impl KernelMachine {
    /// Loads a session's bindings: its objects under fresh identities, and
    /// its variables (`K-SES-002`).
    fn load(&mut self, bindings: &Bindings) -> Result<(), StartError> {
        let refuse = |problem: &str| StartError::Bindings {
            problem: problem.to_string(),
        };
        // Every object takes its place first, so that values can name
        // objects that come later.
        let mut ids: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for id in bindings.objects.keys() {
            ids.insert(*id, self.heap.insert(Obj::List(Vec::new())));
        }
        let rename = |value: &Value| {
            rename(value, &ids).ok_or_else(|| {
                refuse("a binding names an object the bindings do not hold, a closure or a task")
            })
        };
        for (id, object) in &bindings.objects {
            let object = match object {
                Object::List(items) => {
                    Obj::List(items.iter().map(&rename).collect::<Result<_, _>>()?)
                }
                Object::Record(fields) => Obj::Record(
                    fields
                        .iter()
                        .map(|(name, value)| Ok((name.clone(), rename(value)?)))
                        .collect::<Result<_, StartError>>()?,
                ),
                Object::Map(entries) => {
                    let mut table = Table::default();
                    for (key, value) in entries {
                        let key = rename(key)?;
                        let normal =
                            Key::of(&key).ok_or_else(|| refuse("a map key is not a legal key"))?;
                        table.insert(normal, key, rename(value)?);
                    }
                    Obj::Map(table)
                }
                Object::Set(members) => {
                    let mut table = Table::default();
                    for member in members {
                        let member = rename(member)?;
                        let normal = Key::of(&member)
                            .ok_or_else(|| refuse("a set member is not a legal key"))?;
                        table.insert(normal, member, Value::Null);
                    }
                    Obj::Set(table)
                }
                Object::Closure(_) | Object::Variable(_) => {
                    return Err(refuse("a session binding cannot hold a closure"));
                }
            };
            self.heap.memory = self.heap.memory.saturating_add(object_bytes(&object));
            if let Some(slot) = ids.get(id).and_then(|id| self.heap.get_mut(*id)) {
                *slot = object;
            }
        }
        for (name, value) in &bindings.variables {
            let value = rename(value)?;
            self.heap.memory = self.heap.memory.saturating_add(value_bytes(&value));
            self.session.insert(name.clone(), value);
        }
        Ok(())
    }

    /// The run's result with the session's bindings as `main` left them.
    /// A binding that reaches a closure or a task handle is not carried
    /// (`K-SES-003`).
    pub(super) fn finished(&self, result: Datum, finish: bool) -> Finished {
        let mut finished = Finished {
            result,
            finish,
            bindings: Bindings::default(),
            not_carried: Vec::new(),
            closures: Bindings::default(),
        };
        if !self.session_cell {
            return finished;
        }
        for (name, value) in &self.session {
            match self.reach(value) {
                Some(objects) => {
                    finished
                        .bindings
                        .variables
                        .insert(name.clone(), value.clone());
                    for (id, object) in objects {
                        finished.bindings.objects.insert(id, object);
                    }
                }
                None => {
                    finished.not_carried.push(name.clone());
                    if let Some(objects) = self.reach_closures(value) {
                        finished
                            .closures
                            .variables
                            .insert(name.clone(), value.clone());
                        finished.closures.objects.extend(objects);
                    }
                }
            }
        }
        finished
    }

    /// The objects a value reaches, closures and the variables they share
    /// among them, or `None` when it reaches a task handle.
    fn reach_closures(&self, value: &Value) -> Option<BTreeMap<ObjectId, Object>> {
        let view = super::parked::HeapView {
            heap: &self.heap,
            exe: &self.exe,
        };
        let mut objects = BTreeMap::new();
        let mut pending = Vec::new();
        named(value, &mut pending)?;
        while let Some(id) = pending.pop() {
            if objects.contains_key(&id) {
                continue;
            }
            let object = view.data(self.heap.get(id)?)?;
            match &object {
                Object::List(items) | Object::Set(items) => items
                    .iter()
                    .try_for_each(|item| named(item, &mut pending))?,
                Object::Record(fields) => fields
                    .iter()
                    .try_for_each(|(_, value)| named(value, &mut pending))?,
                Object::Map(entries) => entries.iter().try_for_each(|(key, value)| {
                    named(key, &mut pending)?;
                    named(value, &mut pending)
                })?,
                Object::Closure(closure) => {
                    pending.extend(closure.captures.iter().map(|(_, cell)| *cell));
                }
                Object::Variable(value) => named(value, &mut pending)?,
            }
            objects.insert(id, object);
        }
        Some(objects)
    }

    /// The objects a value reaches, as data, or `None` when it reaches a
    /// closure or a task handle.
    fn reach(&self, value: &Value) -> Option<BTreeMap<ObjectId, Object>> {
        let mut objects = BTreeMap::new();
        let mut pending = Vec::new();
        carried(value, &mut pending)?;
        while let Some(id) = pending.pop() {
            if objects.contains_key(&id) {
                continue;
            }
            let object = match self.heap.get(id)? {
                Obj::List(items) => Object::List(items.clone()),
                Obj::Record(fields) => Object::Record(fields.clone()),
                Obj::Map(table) => Object::Map(
                    table
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
                Obj::Set(table) => {
                    Object::Set(table.iter().map(|(member, _)| member.clone()).collect())
                }
                Obj::Closure(_) | Obj::Variable(_) => return None,
            };
            let mut inner = |value: &Value| carried(value, &mut pending);
            match &object {
                Object::List(items) | Object::Set(items) => {
                    items.iter().try_for_each(&mut inner)?
                }
                Object::Record(fields) => fields.iter().try_for_each(|(_, value)| inner(value))?,
                Object::Map(entries) => entries.iter().try_for_each(|(key, value)| {
                    inner(key)?;
                    inner(value)
                })?,
                Object::Closure(_) | Object::Variable(_) => {}
            }
            objects.insert(id, object);
        }
        Some(objects)
    }
}

/// Adds the objects a value names, or `None` when it names a closure or a
/// task.
fn carried(value: &Value, pending: &mut Vec<ObjectId>) -> Option<()> {
    match value {
        Value::Closure(_) | Value::Task(_) | Value::Ref(Identity::Task(_)) => None,
        Value::Ref(Identity::Object(id)) => {
            pending.push(*id);
            Some(())
        }
        Value::Tuple(members) => members
            .iter()
            .try_for_each(|member| carried(member, pending)),
        Value::Error(error) => carried(&error.data, pending),
        other => {
            pending.extend(other.object());
            Some(())
        }
    }
}

/// Adds the objects a value names, closures among them, or `None` when it
/// names a task.
fn named(value: &Value, pending: &mut Vec<ObjectId>) -> Option<()> {
    match value {
        Value::Task(_) | Value::Ref(Identity::Task(_)) => None,
        Value::Ref(Identity::Object(id)) => {
            pending.push(*id);
            Some(())
        }
        Value::Tuple(members) => members.iter().try_for_each(|member| named(member, pending)),
        Value::Error(error) => named(&error.data, pending),
        other => {
            pending.extend(other.object());
            Some(())
        }
    }
}

/// A binding's value with every object identity replaced by the one the
/// heap gave it.
fn rename(value: &Value, ids: &BTreeMap<ObjectId, ObjectId>) -> Option<Value> {
    let id = |id: &ObjectId| ids.get(id).copied();
    Some(match value {
        Value::List(object) => Value::List(id(object)?),
        Value::Map(object) => Value::Map(id(object)?),
        Value::Set(object) => Value::Set(id(object)?),
        Value::Record(object) => Value::Record(id(object)?),
        Value::Ref(Identity::Object(object)) => Value::Ref(Identity::Object(id(object)?)),
        Value::Closure(_) | Value::Task(_) | Value::Ref(Identity::Task(_)) => return None,
        Value::Tuple(members) => Value::Tuple(
            members
                .iter()
                .map(|member| rename(member, ids))
                .collect::<Option<_>>()?,
        ),
        Value::Error(error) => Value::Error(Arc::new(ErrorValue {
            kind: error.kind.clone(),
            message: error.message.clone(),
            data: rename(&error.data, ids)?,
        })),
        other => other.clone(),
    })
}
