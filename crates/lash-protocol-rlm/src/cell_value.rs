//! Kernel values as the JSON a host reads, and host JSON as kernel values.
//!
//! A cell's values leave the run as [`Datum`] trees (a print, a finish
//! value, a tool argument) or stay in the session as [`Value`]s over a heap
//! of [`Object`]s. Hosts, history and prompts read JSON. Two readings exist
//! and they differ on purpose:
//!
//! - [`datum_json`] and [`binding_json`] are total: every kernel value has a
//!   JSON reading, so a print never fails for what it prints. A kind JSON
//!   has no spelling for is read as a record that names its kind under
//!   [`KIND_KEY`]. These readings are typed facts for hosts and renderers;
//!   nothing reads them back into a run.
//! - A tool's arguments use the broker's strict writer
//!   (`lash_vm_broker::kernel::datum_to_json`), which refuses what JSON
//!   cannot carry.
//!
//! [`bind_json`] goes the other way, for values a host seeds a session
//! with: each array and object becomes a fresh heap object.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Datum, Float, Integer, NumberPolicy, Object, ObjectId, Value};
use serde_json::{Map, Number, Value as Json};

/// The field that names the kind of a value JSON has no spelling for.
pub(crate) const KIND_KEY: &str = "$kind";

fn kinded(kind: &str, fields: impl IntoIterator<Item = (&'static str, Json)>) -> Json {
    let mut object = Map::new();
    object.insert(KIND_KEY.to_owned(), Json::String(kind.to_owned()));
    for (name, value) in fields {
        object.insert(name.to_owned(), value);
    }
    Json::Object(object)
}

/// An integer as a JSON number when one holds it exactly, else as its
/// decimal digits under the kind `int`.
fn integer_json(value: &Integer) -> Json {
    let digits = value.to_string();
    if let Ok(small) = digits.parse::<i64>() {
        return Json::Number(small.into());
    }
    if let Ok(small) = digits.parse::<u64>() {
        return Json::Number(small.into());
    }
    kinded("int", [("digits", Json::String(digits))])
}

/// A float as a JSON number, a whole one without a fraction; a non-finite
/// one under the kind `float`.
fn float_json(value: Float) -> Json {
    let float = value.get();
    if !float.is_finite() {
        return kinded("float", [("value", Json::String(value.to_string()))]);
    }
    // 2^53: every whole float below it is one exact integer.
    if float.fract() == 0.0 && float.abs() < 9_007_199_254_740_992.0 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a whole float below 2^53 is an exact i64"
        )]
        return Json::Number((float as i64).into());
    }
    Number::from_f64(float).map_or(Json::Null, Json::Number)
}

fn entries_json<K, V>(
    entries: impl Iterator<Item = (K, V)>,
    key_text: impl Fn(&K) -> Option<String>,
    key_json: impl Fn(&K) -> Json,
    value_json: impl Fn(&V) -> Json,
) -> Json {
    let entries: Vec<(K, V)> = entries.collect();
    let texts: Option<Vec<String>> = entries.iter().map(|(key, _)| key_text(key)).collect();
    match texts {
        // A map keyed by texts reads as an object.
        Some(texts) => Json::Object(
            texts
                .into_iter()
                .zip(entries.iter().map(|(_, value)| value_json(value)))
                .collect(),
        ),
        None => kinded(
            "map",
            [(
                "entries",
                Json::Array(
                    entries
                        .iter()
                        .map(|(key, value)| Json::Array(vec![key_json(key), value_json(value)]))
                        .collect(),
                ),
            )],
        ),
    }
}

/// A value copied out of a run, as JSON. Total: see the module docs.
pub(crate) fn datum_json(value: &Datum) -> Json {
    match value {
        Datum::Null | Datum::Absent => Json::Null,
        Datum::Bool(value) => Json::Bool(*value),
        Datum::Int(value) => integer_json(value),
        Datum::Float(value) => float_json(*value),
        Datum::Number(token) => serde_json::from_str(token.as_str())
            .unwrap_or_else(|_| Json::String(token.as_str().to_owned())),
        Datum::Text(text) => Json::String(text.clone()),
        Datum::Bytes(bytes) => kinded(
            "bytes",
            [("hex", serde_json::to_value(bytes).unwrap_or(Json::Null))],
        ),
        Datum::Timestamp(at) => kinded(
            "timestamp",
            [("nanoseconds", Json::String(at.nanoseconds.to_string()))],
        ),
        Datum::Tuple(items) | Datum::List(items) => {
            Json::Array(items.iter().map(datum_json).collect())
        }
        Datum::Set(members) => kinded(
            "set",
            [(
                "members",
                Json::Array(members.iter().map(datum_json).collect()),
            )],
        ),
        Datum::Map(entries) => entries_json(
            entries.iter().map(|(key, value)| (key, value)),
            |key| match key {
                Datum::Text(text) => Some(text.clone()),
                _ => None,
            },
            |key| datum_json(key),
            |value| datum_json(value),
        ),
        Datum::Record(fields) => Json::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), datum_json(value)))
                .collect(),
        ),
        Datum::Error(error) => kinded(
            "error",
            [
                ("kind", Json::String(error.kind.clone())),
                ("message", Json::String(error.message.clone())),
                ("data", datum_json(&error.data)),
            ],
        ),
        Datum::Function(name) => kinded("function", [("name", Json::String(name.to_string()))]),
        Datum::Handle(handle) => kinded(
            "handle",
            [
                ("handle", Json::String(handle.kind.clone())),
                ("id", Json::String(handle.id.clone())),
            ],
        ),
    }
}

/// A session binding as JSON, read through the session's heap. An object
/// met again on the path that reached it is a cycle, read as the kind
/// `cycle`; a shared object that is no cycle is read at each place it
/// appears. Total: see the module docs.
pub(crate) fn binding_json(value: &Value, objects: &BTreeMap<ObjectId, Object>) -> Json {
    binding_json_on(value, objects, &mut BTreeSet::new())
}

fn binding_json_on(
    value: &Value,
    objects: &BTreeMap<ObjectId, Object>,
    path: &mut BTreeSet<ObjectId>,
) -> Json {
    let object = |id: &ObjectId, path: &mut BTreeSet<ObjectId>| -> Json {
        if !path.insert(*id) {
            return kinded("cycle", []);
        }
        let json = match objects.get(id) {
            Some(Object::List(items)) => Json::Array(
                items
                    .iter()
                    .map(|item| binding_json_on(item, objects, path))
                    .collect(),
            ),
            Some(Object::Set(members)) => {
                let members = members
                    .iter()
                    .map(|member| binding_json_on(member, objects, path))
                    .collect();
                kinded("set", [("members", Json::Array(members))])
            }
            Some(Object::Record(fields)) => Json::Object(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), binding_json_on(value, objects, path)))
                    .collect(),
            ),
            Some(Object::Map(entries)) => {
                let read: Vec<(Json, Json, Option<String>)> = entries
                    .iter()
                    .map(|(key, value)| {
                        let text = match key {
                            Value::Text(text) => Some(text.to_string()),
                            _ => None,
                        };
                        (
                            binding_json_on(key, objects, path),
                            binding_json_on(value, objects, path),
                            text,
                        )
                    })
                    .collect();
                entries_json(
                    read.into_iter()
                        .map(|(key, value, text)| ((key, text), value)),
                    |(_, text)| text.clone(),
                    |(key, _)| key.clone(),
                    Json::clone,
                )
            }
            // A session binding never reaches these (`K-SES-003`).
            Some(Object::Closure(_)) => kinded("closure", []),
            Some(Object::Variable(value)) => binding_json_on(value, objects, path),
            None => Json::Null,
        };
        path.remove(id);
        json
    };
    match value {
        Value::Null | Value::Absent => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int(value) => integer_json(value),
        Value::Float(value) => float_json(*value),
        Value::Text(text) => Json::String(text.to_string()),
        Value::Bytes(bytes) => kinded(
            "bytes",
            [("hex", serde_json::to_value(bytes).unwrap_or(Json::Null))],
        ),
        Value::Timestamp(at) => kinded(
            "timestamp",
            [("nanoseconds", Json::String(at.nanoseconds.to_string()))],
        ),
        Value::Tuple(items) => Json::Array(
            items
                .iter()
                .map(|item| binding_json_on(item, objects, path))
                .collect(),
        ),
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => object(id, path),
        Value::Closure(_) => kinded("closure", []),
        Value::Error(error) => kinded(
            "error",
            [
                ("kind", Json::String(error.kind.clone())),
                ("message", Json::String(error.message.clone())),
                ("data", binding_json_on(&error.data, objects, path)),
            ],
        ),
        Value::Task(_) => kinded("task", []),
        Value::Function(name) => kinded("function", [("name", Json::String(name.to_string()))]),
        Value::Handle(handle) => kinded(
            "handle",
            [
                ("handle", Json::String(handle.kind.clone())),
                ("id", Json::String(handle.id.clone())),
            ],
        ),
        Value::Ref(_) => kinded("ref", []),
    }
}

/// A host's JSON as a kernel value: an array is a fresh list and an object
/// a fresh record, each allocated by `allocate` and written to `objects`.
/// A number is decoded by `numbers`, the session dialect's policy for a
/// bare number (`K-EFF-005`).
pub(crate) fn bind_json(
    json: &Json,
    numbers: NumberPolicy,
    allocate: &mut dyn FnMut() -> ObjectId,
    objects: &mut BTreeMap<ObjectId, Object>,
) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(value) => Value::Bool(*value),
        Json::Number(number) => {
            let integer = match numbers {
                NumberPolicy::Float => None,
                NumberPolicy::BySpelling => number
                    .as_i64()
                    .map(Integer::new)
                    .or_else(|| number.as_u64().map(Integer::new)),
            };
            match integer {
                Some(integer) => Value::Int(integer),
                None => Value::Float(Float::new(number.as_f64().unwrap_or(f64::NAN))),
            }
        }
        Json::String(text) => Value::text(text.as_str()),
        Json::Array(items) => {
            let id = allocate();
            let items = items
                .iter()
                .map(|item| bind_json(item, numbers, allocate, objects))
                .collect();
            objects.insert(id, Object::List(items));
            Value::List(id)
        }
        Json::Object(fields) => {
            let id = allocate();
            let fields = fields
                .iter()
                .map(|(name, value)| (name.clone(), bind_json(value, numbers, allocate, objects)))
                .collect();
            objects.insert(id, Object::Record(fields));
            Value::Record(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kind JSON has no spelling for is read as a record naming its
    /// kind, and a map keyed by texts as an object.
    #[test]
    fn every_datum_has_a_json_reading() {
        let map = Datum::Map(vec![
            (Datum::Text("b".into()), Datum::Int(Integer::new(1))),
            (Datum::Text("a".into()), Datum::Float(Float::new(2.5))),
        ]);
        assert_eq!(datum_json(&map), serde_json::json!({"b": 1, "a": 2.5}));
        let keyed = Datum::Map(vec![(Datum::Int(Integer::new(1)), Datum::Absent)]);
        assert_eq!(
            datum_json(&keyed),
            serde_json::json!({"$kind": "map", "entries": [[1, null]]})
        );
        assert_eq!(
            datum_json(&Datum::Set(vec![Datum::Bool(true)])),
            serde_json::json!({"$kind": "set", "members": [true]})
        );
        assert_eq!(
            datum_json(&Datum::Float(Float::new(3.0))),
            serde_json::json!(3)
        );
        assert_eq!(
            datum_json(&Datum::Float(Float::new(f64::INFINITY)))[KIND_KEY],
            "float"
        );
    }

    /// A seeded value reads back as the JSON it was seeded from, and a
    /// cycle in a session's heap is read as one rather than followed.
    #[test]
    fn seeded_json_reads_back_and_a_cycle_ends() {
        let mut objects = BTreeMap::new();
        let mut next = 0;
        let mut allocate = || {
            next += 1;
            ObjectId(next)
        };
        let seeded = serde_json::json!({"rows": [1, 2.5, "x"], "ok": true});
        let value = bind_json(
            &seeded,
            NumberPolicy::BySpelling,
            &mut allocate,
            &mut objects,
        );
        assert_eq!(binding_json(&value, &objects), seeded);

        let id = ObjectId(99);
        objects.insert(id, Object::List(vec![Value::List(id)]));
        assert_eq!(
            binding_json(&Value::List(id), &objects),
            serde_json::json!([{"$kind": "cycle"}])
        );
    }
}
