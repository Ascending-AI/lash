//! What each native implementation answers, as one digest (FIG-5799).
//!
//! A native function's definition states its signature, charge and guard;
//! the Rust code behind it is not part of its identity. A released helper
//! set retains the native functions its bodies call by identity, so the
//! build has to notice when the code behind an identity starts answering
//! otherwise. The probe calls every native implementation of a registry
//! with a fixed set of arguments drawn from its signature and hashes what
//! each call answers: its value, its error, its guard or memory refusal,
//! and the work it counted. A native is a function of its arguments
//! (`K-LIB-006`), so the digest changes only when its code does, for an
//! argument the probe tries.
//!
//! The build script includes this file (`build.rs`): it fingerprints the
//! registry the worker ships and compares the digests with the ones each
//! released helper set recorded.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::ControlFlow;

use lash_kernel_doc::{
    Bytes, Element, ErrorValue, Float, FunctionId, FunctionRegistry, Integer, NativeCall,
    NativeError, NativeHeap, Object, ObjectId, Timestamp, Type, Value, WorkCounter,
};

/// How many argument lists one native is called with, at most.
const CALLS: usize = 64;
/// The work one probe call may count.
const WORK: u64 = 100_000;
/// The room one probe call may reserve, in the probe heap's units.
const ROOM: u64 = 1 << 20;
/// How deep a sample nests lists, maps, sets and records.
const DEPTH: usize = 2;

/// The digest of what every native implementation `registry` holds answers,
/// by function, as lower-case hexadecimal.
pub(crate) fn fingerprints(registry: &FunctionRegistry) -> BTreeMap<FunctionId, String> {
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let fingerprints = registry
        .iter()
        .filter_map(|(function, registered)| {
            let native = registered.native.as_ref()?;
            Some((
                *function,
                fingerprint(function, registered, native.as_ref()),
            ))
        })
        .collect();
    std::panic::set_hook(quiet);
    fingerprints
}

fn fingerprint(
    function: &FunctionId,
    registered: &lash_kernel_doc::RegisteredFunction,
    native: &dyn lash_kernel_doc::NativeFunction,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(function.as_bytes());
    let params = &registered.definition.signature.params;
    let mut heap = ProbeHeap::default();
    let choices: Vec<Vec<Value>> = params
        .iter()
        .map(|param| {
            let mut values = samples(&param.ty, &mut heap, DEPTH);
            if param.optional {
                values.push(Value::Absent);
            }
            values
        })
        .collect();
    let objects = heap.objects.len();
    for args in arguments(&choices) {
        heap.objects.truncate(objects);
        heap.reserved = 0;
        let mut counter = WorkCounter::new(Some(WORK));
        let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native.call(NativeCall {
                args: &args,
                heap: &mut heap,
                counter: &mut counter,
            })
        }));
        let mut line = String::new();
        match answered {
            Ok(Ok(value)) => {
                line.push_str("ok ");
                heap.write(&value, DEPTH + 4, &mut line);
            }
            Ok(Err(NativeError::Raised(error))) => {
                let _ = write!(line, "raised {} {}", error.kind, error.message);
            }
            Ok(Err(NativeError::Guard(guard))) => {
                let _ = write!(line, "guard {}", guard.limit);
            }
            Ok(Err(NativeError::Memory)) => line.push_str("memory"),
            Err(_) => line.push_str("panicked"),
        }
        let _ = writeln!(line, " spent {}", counter.spent());
        hasher.update(line.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Up to [`CALLS`] argument lists from `choices`, one value per parameter:
/// every combination when there are that few, the first parameter varying
/// fastest.
fn arguments(choices: &[Vec<Value>]) -> Vec<Vec<Value>> {
    if choices.iter().any(Vec::is_empty) {
        return Vec::new();
    }
    let total = choices
        .iter()
        .try_fold(1usize, |product, values| product.checked_mul(values.len()))
        .unwrap_or(usize::MAX);
    (0..total.min(CALLS))
        .map(|mut index| {
            choices
                .iter()
                .map(|values| {
                    let value = values[index % values.len()].clone();
                    index /= values.len();
                    value
                })
                .collect()
        })
        .collect()
}

/// A few values of `ty`, the objects among them allocated in `heap`.
fn samples(ty: &Type, heap: &mut ProbeHeap, depth: usize) -> Vec<Value> {
    let int = |value: i64| Value::Int(Integer::from(value));
    let float = |value: f64| Value::Float(Float::new(value));
    match ty {
        Type::Any => {
            let mut values = vec![
                Value::Null,
                Value::Bool(true),
                int(3),
                float(-1.5),
                Value::text("ab"),
            ];
            if depth > 0 {
                let list = heap.object(Object::List(vec![int(1), Value::text("x")]));
                values.push(Value::List(list));
            }
            values
        }
        Type::Null => vec![Value::Null],
        Type::Absent => vec![Value::Absent],
        Type::Bool => vec![Value::Bool(false), Value::Bool(true)],
        Type::Int => vec![
            int(0),
            int(1),
            int(-7),
            Value::Int(Integer::new(beyond_a_word())),
        ],
        Type::Float => vec![float(0.0), float(-1.5), float(3.25), float(1e300)],
        Type::Number => vec![int(2), float(0.5), int(-3)],
        Type::Text => vec![
            Value::text(""),
            Value::text("a"),
            Value::text("Hello, Wörld"),
            Value::text("12.5"),
        ],
        Type::Bytes => vec![
            Value::Bytes(Bytes::new(Vec::new())),
            Value::Bytes(Bytes::new(vec![0x61, 0xff])),
        ],
        Type::Timestamp => vec![
            Value::Timestamp(Timestamp {
                nanoseconds: Integer::from(0),
            }),
            Value::Timestamp(Timestamp {
                nanoseconds: Integer::from(1_700_000_000_123_456_789),
            }),
        ],
        Type::Tuple(members) => {
            let tuple: Option<Vec<Value>> = members
                .iter()
                .map(|member| samples(member, heap, depth).into_iter().next())
                .collect();
            tuple
                .map(|tuple| vec![Value::Tuple(tuple.into())])
                .unwrap_or_default()
        }
        Type::Enum(texts) => texts
            .iter()
            .take(3)
            .map(|text| Value::text(text.as_str()))
            .collect(),
        Type::Error => vec![Value::Error(std::sync::Arc::new(ErrorValue::new(
            "type_error",
            "probe",
        )))],
        Type::Union(members) => members
            .iter()
            .flat_map(|member| samples(member, heap, depth).into_iter().take(2))
            .collect(),
        _ if depth == 0 => Vec::new(),
        Type::List(item) => {
            let items: Vec<Value> = samples(item, heap, depth - 1).into_iter().take(2).collect();
            vec![
                Value::List(heap.object(Object::List(Vec::new()))),
                Value::List(heap.object(Object::List(items))),
            ]
        }
        Type::Set(member) => {
            let members: Vec<Value> = samples(member, heap, depth - 1)
                .into_iter()
                .take(1)
                .collect();
            vec![
                Value::Set(heap.object(Object::Set(Vec::new()))),
                Value::Set(heap.object(Object::Set(members))),
            ]
        }
        Type::Map(map) => {
            let keys = samples(&map.key, heap, depth - 1);
            let values = samples(&map.value, heap, depth - 1);
            let entries: Vec<(Value, Value)> = keys.into_iter().zip(values).take(1).collect();
            vec![
                Value::Map(heap.object(Object::Map(Vec::new()))),
                Value::Map(heap.object(Object::Map(entries))),
            ]
        }
        Type::Record(record) => {
            let fields: Option<Vec<(String, Value)>> = record
                .fields
                .iter()
                .filter(|field| !field.optional)
                .map(|field| {
                    samples(&field.ty, heap, depth - 1)
                        .into_iter()
                        .next()
                        .map(|value| (field.name.clone(), value))
                })
                .collect();
            fields
                .map(|fields| vec![Value::Record(heap.object(Object::Record(fields)))])
                .unwrap_or_default()
        }
        Type::Function(_) | Type::Task(_) | Type::Handle(_) => Vec::new(),
    }
}

/// 2 to the 70th: an integer no machine word holds.
fn beyond_a_word() -> i128 {
    1_i128 << 70
}

/// The heap a probe call sees: objects by index, no collection, and a
/// fixed room for what a call reserves.
#[derive(Default)]
struct ProbeHeap {
    objects: Vec<Object>,
    reserved: u64,
}

impl ProbeHeap {
    fn object(&mut self, object: Object) -> ObjectId {
        self.objects.push(object);
        ObjectId(self.objects.len() as u64 - 1)
    }

    fn get(&self, object: ObjectId) -> Option<&Object> {
        usize::try_from(object.0)
            .ok()
            .and_then(|index| self.objects.get(index))
    }

    /// `value` with every object it reaches spelled out, `depth` levels
    /// deep.
    fn write(&self, value: &Value, depth: usize, out: &mut String) {
        let Some(object) = value.object().filter(|_| depth > 0) else {
            let _ = write!(out, "{value:?}");
            return;
        };
        let items = |values: &mut dyn Iterator<Item = &Value>, out: &mut String| {
            for value in values {
                self.write(value, depth - 1, out);
                out.push(',');
            }
        };
        match self.get(object) {
            Some(Object::List(values)) => {
                out.push('[');
                items(&mut values.iter(), out);
                out.push(']');
            }
            Some(Object::Set(values)) => {
                out.push_str("set[");
                items(&mut values.iter(), out);
                out.push(']');
            }
            Some(Object::Map(entries)) => {
                out.push('{');
                items(
                    &mut entries.iter().flat_map(|(key, value)| [key, value]),
                    out,
                );
                out.push('}');
            }
            Some(Object::Record(fields)) => {
                out.push('(');
                for (name, value) in fields {
                    let _ = write!(out, "{name}=");
                    self.write(value, depth - 1, out);
                    out.push(',');
                }
                out.push(')');
            }
            Some(other) => {
                let _ = write!(out, "{other:?}");
            }
            None => out.push_str("missing"),
        }
    }
}

impl NativeHeap for ProbeHeap {
    fn len(&self, object: ObjectId) -> usize {
        match self.get(object) {
            Some(Object::List(values) | Object::Set(values)) => values.len(),
            Some(Object::Map(entries)) => entries.len(),
            Some(Object::Record(fields)) => fields.len(),
            _ => 0,
        }
    }

    fn list_get(&self, list: ObjectId, index: usize) -> Option<Value> {
        match self.get(list)? {
            Object::List(values) => values.get(index).cloned(),
            _ => None,
        }
    }

    fn map_get(&self, map: ObjectId, key: &Value) -> Option<Value> {
        match self.get(map)? {
            Object::Map(entries) => entries
                .iter()
                .find(|(held, _)| held == key)
                .map(|(_, value)| value.clone()),
            _ => None,
        }
    }

    fn set_contains(&self, set: ObjectId, member: &Value) -> bool {
        matches!(self.get(set), Some(Object::Set(members)) if members.contains(member))
    }

    fn record_get(&self, record: ObjectId, field: &str) -> Option<Value> {
        match self.get(record)? {
            Object::Record(fields) => fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, value)| value.clone()),
            _ => None,
        }
    }

    fn visit(&self, object: ObjectId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) {
        let _ = match self.get(object) {
            Some(Object::List(values) | Object::Set(values)) => values
                .iter()
                .try_for_each(|value| visitor(Element::Item(value))),
            Some(Object::Map(entries)) => entries
                .iter()
                .try_for_each(|(key, value)| visitor(Element::Entry { key, value })),
            Some(Object::Record(fields)) => fields
                .iter()
                .try_for_each(|(name, value)| visitor(Element::Field { name, value })),
            _ => ControlFlow::Continue(()),
        };
    }

    fn allocate(&mut self, object: Object) -> Result<ObjectId, NativeError> {
        if matches!(object, Object::Closure(_) | Object::Variable(_)) {
            return Err(NativeError::Raised(ErrorValue::new(
                "type_error",
                "a native function allocates lists, maps, sets and records",
            )));
        }
        Ok(self.object(object))
    }

    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let reserved = self
            .reserved
            .saturating_add(values.saturating_mul(16))
            .saturating_add(bytes);
        if reserved > ROOM {
            return Err(NativeError::Memory);
        }
        self.reserved = reserved;
        Ok(())
    }
}
