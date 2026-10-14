//! Executes one recorded native call, including its prepared entry and bounds.

use lash_kernel_lib::Key;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::ControlFlow;

use lash_kernel_doc::{
    Element, ErrorValue, Measure, NativeCall, NativeError, NativeHeap, Object, ObjectId, Operand,
    PreparedNative, RegisteredFunction, Value, WorkCounter,
};
use serde::{Deserialize, Serialize};

/// A replayable heap snapshot, not a guest stored format. Object identities
/// are retained, including the exact identities in the imported old inputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub args: Vec<Value>,
    pub objects: Vec<Object>,
    pub work: u64,
    pub room: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    pub outcome: String,
    pub spent: u64,
    pub charge: u64,
    pub reservations: Vec<(u64, u64)>,
    pub reserved: u64,
    /// An equivalent prepared entry or delegation has no extra observation.
    pub prepared_difference: Option<String>,
}

#[cfg_attr(
    not(test),
    expect(
        clippy::expect_used,
        reason = "certification only calls registry entries holding a native implementation"
    )
)]
pub(crate) fn answer(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
) -> Answer {
    let native = registered.native.as_ref().expect("a registered native");
    let mut heap = ProbeHeap {
        objects: input.objects.clone(),
        room: input.room,
        ..Default::default()
    };
    let mut counter = WorkCounter::new(Some(input.work));
    let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native.call(NativeCall {
            args: &input.args,
            heap: &mut heap,
            counter: &mut counter,
        })
    }));
    let mut outcome = String::new();
    let result = match called {
        Ok(Ok(value)) => {
            outcome.push_str("ok ");
            heap.write(&value, &mut outcome);
            value
        }
        Ok(Err(NativeError::Raised(error))) => {
            let _ = write!(outcome, "raised {:?} {:?} ", error.kind, error.message);
            heap.write(&error.data, &mut outcome);
            Value::Absent
        }
        Ok(Err(NativeError::Guard(guard))) => {
            let _ = write!(outcome, "guard {}", guard.limit);
            Value::Absent
        }
        Ok(Err(NativeError::Memory)) => {
            outcome.push_str("memory");
            Value::Absent
        }
        Err(_) => {
            outcome.push_str("panicked");
            Value::Absent
        }
    };
    let charge = registered
        .definition
        .charge
        .evaluate(&mut |operand, measure| {
            let value = match operand {
                Operand::Result => &result,
                Operand::Param(name) => registered
                    .definition
                    .signature
                    .params
                    .iter()
                    .position(|param| &param.name == name)
                    .and_then(|index| input.args.get(index))
                    .unwrap_or(&Value::Absent),
            };
            heap.measure(value, measure)
        });
    // Prepared entries are another path to the same native answer. Operation
    // labels and delegation do not change release identity; only a differing
    // answer is recorded. Guarded calls use the counted general entry above.
    let prepared_difference = prepared
        .filter(|_| !outcome.starts_with("guard "))
        .and_then(|prepared| {
            let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (prepared.entry)(&input.args)
            }));
            let mut observed = String::new();
            match called {
                Ok(Some(value)) => {
                    observed.push_str("ok ");
                    heap.write(&value, &mut observed);
                }
                Ok(None) => return None,
                Err(_) => observed.push_str("panicked"),
            }
            (observed != outcome).then_some(observed)
        });
    Answer {
        outcome,
        spent: counter.spent(),
        charge,
        reservations: heap.reservations,
        reserved: heap.reserved,
        prepared_difference,
    }
}

/// The heap a probe call sees: objects by index, no collection, and a
/// fixed room for what a call reserves.
#[derive(Default)]
pub(crate) struct ProbeHeap {
    pub(crate) objects: Vec<Object>,
    pub(crate) reserved: u64,
    pub(crate) room: u64,
    pub(crate) reservations: Vec<(u64, u64)>,
}

impl ProbeHeap {
    pub(crate) fn object(&mut self, object: Object) -> ObjectId {
        self.objects.push(object);
        ObjectId(self.objects.len() as u64 - 1)
    }

    pub(crate) fn get(&self, object: ObjectId) -> Option<&Object> {
        usize::try_from(object.0)
            .ok()
            .and_then(|index| self.objects.get(index))
    }

    /// A value's canonical stored scalars and all reached collection contents.
    /// Object addresses are expanded; explicit identity references remain values.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "typed kernel scalars serialize and the object branch established its identity"
        )
    )]
    fn json(&self, value: &Value, path: &mut BTreeSet<ObjectId>) -> serde_json::Value {
        let values = |values: &[Value], path: &mut BTreeSet<ObjectId>| {
            values
                .iter()
                .map(|value| self.json(value, path))
                .collect::<Vec<_>>()
        };
        match value {
            Value::Tuple(items) => serde_json::json!({"tuple": values(items, path)}),
            Value::Error(error) => serde_json::json!({"error": {
                "kind": error.kind, "message": error.message, "data": self.json(&error.data, path),
            }}),
            _ if value.object().is_some() => {
                let id = value.object().expect("the branch holds an object");
                if !path.insert(id) {
                    return serde_json::json!({"cyclic_object": id});
                }
                let result = match (value, self.get(id)) {
                    (Value::List(_), Some(Object::List(items))) => {
                        serde_json::json!({"list": values(items, path)})
                    }
                    (Value::Set(_), Some(Object::Set(items))) => {
                        serde_json::json!({"set": values(items, path)})
                    }
                    (Value::Map(_), Some(Object::Map(entries))) => {
                        serde_json::json!({"map": entries.iter().map(|(key, value)|
                        [self.json(key, path), self.json(value, path)]).collect::<Vec<_>>() })
                    }
                    (Value::Record(_), Some(Object::Record(fields))) => {
                        serde_json::json!({"record": fields.iter().map(|(name, value)|
                        (name, self.json(value, path))).collect::<Vec<_>>() })
                    }
                    (_, object) => {
                        serde_json::json!({"invalid_object": {"value": value, "object": object}})
                    }
                };
                path.remove(&id);
                result
            }
            _ => serde_json::to_value(value).expect("kernel scalars serialize"),
        }
    }

    fn write(&self, value: &Value, out: &mut String) {
        out.push_str(&self.json(value, &mut BTreeSet::new()).to_string());
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
        let key = Key::new(key.clone()).ok()?;
        match self.get(map)? {
            Object::Map(entries) => entries
                .iter()
                .find(|(held, _)| Key::new(held.clone()).ok().as_ref() == Some(&key))
                .map(|(_, value)| value.clone()),
            _ => None,
        }
    }

    fn set_contains(&self, set: ObjectId, member: &Value) -> bool {
        let Ok(key) = Key::new(member.clone()) else {
            return false;
        };
        matches!(self.get(set), Some(Object::Set(members)) if members.iter().any(|member| Key::new(member.clone()).ok().as_ref() == Some(&key)))
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
        let object = match object {
            Object::Map(entries) => {
                let mut positions = HashMap::<Key, usize>::new();
                let mut kept = Vec::<(Value, Value)>::new();
                for (key, value) in entries {
                    let normalized = Key::new(key.clone()).map_err(|_| {
                        NativeError::Raised(ErrorValue::new(
                            "type_error",
                            "a key is not a legal key",
                        ))
                    })?;
                    if let Some(index) = positions.get(&normalized) {
                        kept[*index].1 = value;
                    } else {
                        positions.insert(normalized, kept.len());
                        kept.push((key, value));
                    }
                }
                Object::Map(kept)
            }
            Object::Set(members) => {
                let mut seen = HashSet::new();
                let mut kept = Vec::new();
                for member in members {
                    if seen.insert(Key::new(member.clone()).map_err(|_| {
                        NativeError::Raised(ErrorValue::new(
                            "type_error",
                            "a key is not a legal key",
                        ))
                    })?) {
                        kept.push(member);
                    }
                }
                Object::Set(kept)
            }
            Object::Record(fields) => {
                let mut kept = Vec::<(String, Value)>::new();
                for (name, value) in fields {
                    if let Some(field) = kept.iter_mut().find(|(field, _)| *field == name) {
                        field.1 = value;
                    } else {
                        kept.push((name, value));
                    }
                }
                Object::Record(kept)
            }
            Object::Closure(_) | Object::Variable(_) => {
                return Err(NativeError::Raised(ErrorValue::new(
                    "type_error",
                    "a native function allocates lists, maps, sets and records",
                )));
            }
            object => object,
        };
        Ok(self.object(object))
    }

    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let reserved = self
            .reserved
            .saturating_add(values.saturating_mul(16))
            .saturating_add(bytes);
        self.reservations.push((values, bytes));
        if reserved > self.room {
            return Err(NativeError::Memory);
        }
        self.reserved = reserved;
        Ok(())
    }
}

impl ProbeHeap {
    fn size(&self, value: &Value) -> u64 {
        1 + match value {
            Value::Int(value) => value.bits().div_ceil(64),
            Value::Text(value) => value.len() as u64,
            Value::Bytes(value) => value.as_slice().len() as u64,
            Value::Tuple(values) => values.len() as u64,
            Value::Error(error) => (error.kind.len() + error.message.len()) as u64,
            Value::Map(id) => 2 * self.len(*id) as u64,
            Value::List(id) | Value::Set(id) | Value::Record(id) => self.len(*id) as u64,
            _ => 0,
        }
    }

    fn measure(&self, value: &Value, measure: Measure) -> u64 {
        match measure {
            Measure::Size => self.size(value),
            Measure::Magnitude => match value {
                Value::Int(value) if !value.is_negative() => {
                    value.to_string().parse().unwrap_or(u64::MAX)
                }
                Value::Float(value) if value.get().is_finite() && value.get() >= 0.0 => {
                    value.get() as u64
                }
                _ => 0,
            },
            Measure::DeepSize | Measure::NestedSize => {
                let mut pending = vec![value.clone()];
                let mut seen = BTreeSet::new();
                let mut total = 0_u64;
                let mut root = true;
                while let Some(value) = pending.pop() {
                    let nested = measure == Measure::NestedSize;
                    if let Some(id) = value.object() {
                        if nested {
                            if !root {
                                total = total.saturating_add(1);
                            }
                            root = false;
                            continue;
                        }
                        if !seen.insert(id) {
                            continue;
                        }
                        self.visit(id, &mut |element| {
                            match element {
                                Element::Item(value) | Element::Field { value, .. } => {
                                    pending.push(value.clone())
                                }
                                Element::Entry { key, value } => {
                                    pending.push(key.clone());
                                    pending.push(value.clone());
                                }
                            }
                            ControlFlow::Continue(())
                        });
                    }
                    if !nested || !root {
                        total = total.saturating_add(self.size(&value));
                    }
                    root = false;
                    match value {
                        Value::Tuple(values) => pending.extend(values.iter().cloned()),
                        Value::Error(error) => pending.push(error.data.clone()),
                        _ => {}
                    }
                }
                total
            }
        }
    }
}
