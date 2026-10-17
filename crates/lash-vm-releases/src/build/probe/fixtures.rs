//! Recorded inputs are testing/build data, built into an arena as a run holds them.
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::{EntityId, FamilyId, LogicalBytes};
use lash_kernel_doc::{
    Bytes, Float, Handle, Integer, KernelDataLimits, Name, NewObject, Nursery, TaskId, Timestamp,
    Value,
};
use lash_kernel_state::arena::Arena;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordedValue {
    Null,
    Absent,
    Bool(bool),
    Int(Integer),
    Float(Float),
    Text(String),
    Bytes(Bytes),
    Timestamp(Timestamp),
    Tuple(Vec<RecordedValue>),
    List(u64),
    Map(u64),
    Set(u64),
    Record(u64),
    Error(Arc<RecordedError>),
    Task(TaskId),
    Function(Name),
    Handle(Arc<Handle>),
    Ref(RecordedIdentity),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordedIdentity {
    Object(u64),
    Task(TaskId),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedError {
    pub kind: String,
    pub message: String,
    pub data: RecordedValue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordedObject {
    List(Vec<RecordedValue>),
    Set(Vec<RecordedValue>),
    Map(Vec<(RecordedValue, RecordedValue)>),
    Record(Vec<(String, RecordedValue)>),
}
pub(crate) struct Replay {
    pub(crate) arena: Arena,
    pub(crate) args: Vec<Value>,
    /// Each fixture object's handle and its recorded ordinal.
    pub(crate) identities: Vec<(ArenaHandle, u64)>,
    /// The ordinal the next fresh object takes.
    pub(crate) next: u64,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl Replay {
    pub(crate) fn load(input: &super::Input) -> Result<Self, String> {
        // Fixture snapshots share storage across cases. Only references reached
        // by this call are instantiated; unused fixture rows are not allocations.
        fn reached(
            value: &RecordedValue,
            objects: &[RecordedObject],
            seen: &mut BTreeSet<u64>,
        ) -> Result<(), String> {
            let object = match value {
                RecordedValue::List(id)
                | RecordedValue::Map(id)
                | RecordedValue::Set(id)
                | RecordedValue::Record(id)
                | RecordedValue::Ref(RecordedIdentity::Object(id)) => Some(*id),
                RecordedValue::Tuple(values) => {
                    for v in values {
                        reached(v, objects, seen)?;
                    }
                    None
                }
                RecordedValue::Error(error) => {
                    reached(&error.data, objects, seen)?;
                    None
                }
                _ => None,
            };
            if let Some(id) = object
                && seen.insert(id)
            {
                match objects
                    .get(id as usize)
                    .ok_or_else(|| format!("fixture object {id} is absent"))?
                {
                    RecordedObject::List(values) | RecordedObject::Set(values) => {
                        for v in values {
                            reached(v, objects, seen)?;
                        }
                    }
                    RecordedObject::Map(entries) => {
                        for (k, v) in entries {
                            reached(k, objects, seen)?;
                            reached(v, objects, seen)?;
                        }
                    }
                    RecordedObject::Record(fields) => {
                        for (_, v) in fields {
                            reached(v, objects, seen)?;
                        }
                    }
                }
            }
            Ok(())
        }
        let mut seen = BTreeSet::new();
        for value in &input.args {
            reached(value, &input.objects, &mut seen)?;
        }
        let mut arena = Arena::new();
        let mut ordinal = 0u64;
        let mut ids = BTreeMap::new();
        for index in &seen {
            let object = match &input.objects[*index as usize] {
                RecordedObject::List(_) => NewObject::List(Vec::new()),
                RecordedObject::Map(_) => NewObject::Map(Vec::new()),
                RecordedObject::Set(_) => NewObject::Set(Vec::new()),
                RecordedObject::Record(_) => NewObject::Record(Vec::new()),
            };
            let (value, new) = {
                let mut nursery = Nursery::new(arena.objects(), LogicalBytes(u64::MAX));
                let value = nursery.allocate(object);
                (value, nursery.into_new())
            };
            arena
                .install(new, &mut || {
                    ordinal += 1;
                    Ok(EntityId {
                        family: FamilyId::PRIMARY,
                        ordinal,
                    })
                })
                .map_err(error)?;
            ids.insert(*index, value);
        }
        for index in seen {
            let object = ids[&index];
            let handle = object.handle().ok_or("a fixture object has no handle")?;
            match &input.objects[index as usize] {
                RecordedObject::List(values) => {
                    for (at, value) in values.iter().enumerate() {
                        let value = decode(&mut arena, value, &ids)?;
                        arena.list_set(handle, at as u64, value).map_err(error)?;
                    }
                }
                RecordedObject::Set(values) => {
                    for value in values {
                        let value = decode(&mut arena, value, &ids)?;
                        arena
                            .table_write(handle, value, Value::Absent)
                            .map_err(error)?;
                    }
                }
                RecordedObject::Map(entries) => {
                    for (key, value) in entries {
                        let key = decode(&mut arena, key, &ids)?;
                        let value = decode(&mut arena, value, &ids)?;
                        arena.table_write(handle, key, value).map_err(error)?;
                    }
                }
                RecordedObject::Record(fields) => {
                    for (name, value) in fields {
                        let name = arena.text(name);
                        let value = decode(&mut arena, value, &ids)?;
                        arena.table_write(handle, name, value).map_err(error)?;
                    }
                }
            }
        }
        let args = input
            .args
            .iter()
            .map(|v| decode(&mut arena, v, &ids))
            .collect::<Result<Vec<_>, _>>()?;
        let identities = ids
            .into_iter()
            .filter_map(|(ordinal, value)| Some((value.handle()?, ordinal)))
            .collect();
        Ok(Self {
            arena,
            args,
            identities,
            next: ordinal,
        })
    }

    /// The recorded ordinal of fixture object `handle`.
    pub(crate) fn ordinal(&self, handle: ArenaHandle) -> Option<u64> {
        self.identities
            .iter()
            .find(|(held, _)| *held == handle)
            .map(|(_, ordinal)| *ordinal)
    }
}
fn decode(
    arena: &mut Arena,
    v: &RecordedValue,
    ids: &BTreeMap<u64, Value>,
) -> Result<Value, String> {
    let id = |n: &u64| {
        ids.get(n)
            .copied()
            .ok_or_else(|| format!("fixture object {n} not reached"))
    };
    Ok(match v {
        RecordedValue::Null => Value::Null,
        RecordedValue::Absent => Value::Absent,
        RecordedValue::Bool(v) => Value::Bool(*v),
        RecordedValue::Int(v) => arena.int(v.clone()),
        RecordedValue::Float(v) => Value::Float(*v),
        RecordedValue::Text(v) => arena.text(v),
        RecordedValue::Bytes(v) => arena.bytes(v.as_slice()),
        RecordedValue::Timestamp(v) => arena.timestamp(v.nanoseconds.clone()),
        RecordedValue::Function(v) => arena.function(v.as_str()),
        RecordedValue::Handle(v) => arena.host_handle((**v).clone()),
        RecordedValue::Task(v) => Value::Task(*v),
        RecordedValue::List(v)
        | RecordedValue::Map(v)
        | RecordedValue::Set(v)
        | RecordedValue::Record(v) => id(v)?,
        RecordedValue::Tuple(values) => {
            let values = values
                .iter()
                .map(|v| decode(arena, v, ids))
                .collect::<Result<Vec<_>, _>>()?;
            arena
                .tuple(values, KernelDataLimits::standard().value_depth())
                .map_err(|_| "a fixture tuple nests too deep".to_owned())?
        }
        RecordedValue::Error(v) => {
            let data = decode(arena, &v.data, ids)?;
            arena.error(&v.kind, &v.message, data)
        }
        RecordedValue::Ref(RecordedIdentity::Object(v)) => match id(v)?.handle() {
            Some(handle) => Value::ObjectRef(handle),
            None => return Err(format!("fixture object {v} has no handle")),
        },
        RecordedValue::Ref(RecordedIdentity::Task(v)) => Value::TaskRef(*v),
    })
}
