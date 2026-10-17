//! Recorded inputs are testing/build data, resolved by ordinary heap operations.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Bytes, Float, Handle, Integer, Name, TaskId, Timestamp, Value};
use lash_kernel_state::paged::{census_impl::Census, tree::Tree, *};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
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
#[derive(Default)]
struct Source(RefCell<BTreeMap<PageId, PageAnswer>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.0
                    .borrow()
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut bytes = body.kind.0.to_be_bytes().to_vec();
        bytes.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&bytes)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
pub(crate) fn fresh() -> Result<PagedHeap, String> {
    let mut heap = PagedHeap::fresh(Box::<Source>::default(), EditEpoch(1));
    let census =
        Census::restore(heap.certificate(), LogicalBytes(1 << 40)).map_err(|e| e.to_string())?;
    heap.set_census(Box::new(census))
        .map_err(|e| e.to_string())?;
    Ok(heap)
}
pub(crate) struct Replay {
    pub(crate) heap: PagedHeap,
    pub(crate) args: Vec<Value>,
    pub(crate) identities: BTreeMap<EntityId, u64>,
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
        let mut heap = fresh()?;
        let mut ids = BTreeMap::new();
        for index in &seen {
            let object = match &input.objects[*index as usize] {
                RecordedObject::List(_) => Object::List(Vec::new()),
                RecordedObject::Map(_) => Object::Map(Vec::new()),
                RecordedObject::Set(_) => Object::Set(Vec::new()),
                RecordedObject::Record(_) => Object::Record(Vec::new()),
            };
            ids.insert(*index, heap.allocate(object).map_err(error)?);
        }
        for index in seen {
            let id = ids[&index];
            match &input.objects[index as usize] {
                RecordedObject::List(values) => {
                    for (at, value) in values.iter().enumerate() {
                        let value = decode(&mut heap, value, &ids)?;
                        heap.list_write(id, at as u64, value).map_err(error)?;
                    }
                }
                RecordedObject::Set(values) => {
                    for value in values {
                        let value = decode(&mut heap, value, &ids)?;
                        heap.set_write(id, value, true).map_err(error)?;
                    }
                }
                RecordedObject::Map(entries) => {
                    for (key, value) in entries {
                        let key = decode(&mut heap, key, &ids)?;
                        let value = decode(&mut heap, value, &ids)?;
                        heap.map_write(id, key, value).map_err(error)?;
                    }
                }
                RecordedObject::Record(fields) => {
                    for (name, value) in fields {
                        let value = decode(&mut heap, value, &ids)?;
                        heap.record_write(id, FieldName::Static(name), value)
                            .map_err(error)?;
                    }
                }
            }
        }
        let args = input
            .args
            .iter()
            .map(|v| decode(&mut heap, v, &ids))
            .collect::<Result<Vec<_>, _>>()?;
        heap.set_execution_roots(Tree::new(false), Tree::new(false), &args, &[], 0, &[])
            .map_err(error)?;
        let certificate = heap.certificate();
        let bound = LogicalBytes(
            certificate
                .upper
                .0
                .checked_add(input.room)
                .ok_or("fixture bound overflow")?,
        );
        heap.set_census(Box::new(
            Census::restore(certificate, bound).map_err(error)?,
        ))
        .map_err(error)?;
        Ok(Self {
            heap,
            args,
            identities: ids.into_iter().map(|(ordinal, id)| (id, ordinal)).collect(),
        })
    }
}
fn decode(
    heap: &mut PagedHeap,
    v: &RecordedValue,
    ids: &BTreeMap<u64, EntityId>,
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
        RecordedValue::Int(v) => Value::Int(heap.int(v.clone()).map_err(error)?),
        RecordedValue::Float(v) => Value::Float(*v),
        RecordedValue::Text(v) => Value::Text(heap.text(v).map_err(error)?),
        RecordedValue::Bytes(v) => Value::Bytes(heap.bytes(v.as_slice()).map_err(error)?),
        RecordedValue::Timestamp(v) => Value::Timestamp(v.clone()),
        RecordedValue::Function(v) => Value::Function(heap.text(v.as_str()).map_err(error)?),
        RecordedValue::Handle(v) => Value::Handle(v.clone()),
        RecordedValue::Task(v) => Value::Task(*v),
        RecordedValue::List(v) => Value::List(id(v)?),
        RecordedValue::Map(v) => Value::Map(id(v)?),
        RecordedValue::Set(v) => Value::Set(id(v)?),
        RecordedValue::Record(v) => Value::Record(id(v)?),
        RecordedValue::Tuple(values) => {
            let values = values
                .iter()
                .map(|v| decode(heap, v, ids))
                .collect::<Result<Vec<_>, _>>()?;
            Value::Tuple(heap.tuple(values).map_err(error)?)
        }
        RecordedValue::Error(v) => {
            let data = decode(heap, &v.data, ids)?;
            let mut e = ErrorRef::new(&v.kind, &v.message);
            e.data = Payload::Resident(Arc::new(data));
            heap.canonical_value(Value::Error(Arc::new(e)))
                .map_err(error)?
        }
        RecordedValue::Ref(RecordedIdentity::Object(v)) => {
            Value::Ref(lash_kernel_doc::Identity::Object(id(v)?))
        }
        RecordedValue::Ref(RecordedIdentity::Task(v)) => {
            Value::Ref(lash_kernel_doc::Identity::Task(*v))
        }
    })
}
