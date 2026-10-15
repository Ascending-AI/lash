//! Kernel values as the JSON a host reads, and host JSON as kernel values.
//!
//! A cell's values leave the run as [`Datum`] trees (a print, a finish
//! value, a tool argument, a session binding a host reads); the session
//! keeps them on its heap's pages. Hosts, history and prompts read JSON.
//! Two readings exist and they differ on purpose:
//!
//! - [`datum_json`] is total: every kernel value has a
//!   JSON reading, so a print never fails for what it prints. A kind JSON
//!   has no spelling for is read as a record that names its kind under
//!   [`KIND_KEY`]. These readings are typed facts for hosts and renderers;
//!   nothing reads them back into a run.
//! - A tool's arguments use the broker's strict writer
//!   (`lash_vm_broker::kernel::datum_to_json`), which refuses what JSON
//!   cannot carry.
//!
//! Both spell a float as [`Float::json_number`] does.
//!
//! [`json_datum`] goes the other way, for values a host seeds a session
//! with.

use lash_kernel_doc::{Datum, Float, Integer, NumberPolicy};
use serde_json::{Map, Value as Json};

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

/// A float as a JSON number ([`Float::json_number`]); a non-finite one
/// under the kind `float`.
fn float_json(value: Float) -> Json {
    value.json_number().map_or_else(
        || kinded("float", [("value", Json::String(value.to_string()))]),
        Json::Number,
    )
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

/// A host's JSON as data a session binds: an array is a list and an object
/// a record. A number is decoded by `numbers`, the session dialect's policy
/// for a bare number (`K-EFF-005`).
pub(crate) fn json_datum(json: &Json, numbers: NumberPolicy) -> Datum {
    match json {
        Json::Null => Datum::Null,
        Json::Bool(value) => Datum::Bool(*value),
        Json::Number(number) => {
            let integer = match numbers {
                NumberPolicy::Float => None,
                NumberPolicy::BySpelling => number
                    .as_i64()
                    .map(Integer::new)
                    .or_else(|| number.as_u64().map(Integer::new)),
            };
            match integer {
                Some(integer) => Datum::Int(integer),
                None => Datum::Float(Float::new(number.as_f64().unwrap_or(f64::NAN))),
            }
        }
        Json::String(text) => Datum::Text(text.clone()),
        Json::Array(items) => {
            Datum::List(items.iter().map(|item| json_datum(item, numbers)).collect())
        }
        Json::Object(fields) => Datum::Record(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), json_datum(value, numbers)))
                .collect(),
        ),
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

    /// A seeded value reads back as the JSON it was seeded from.
    #[test]
    fn seeded_json_reads_back() {
        let seeded = serde_json::json!({"rows": [1, 2.5, "x"], "ok": true});
        let datum = json_datum(&seeded, NumberPolicy::BySpelling);
        assert_eq!(datum_json(&datum), seeded);
    }
}
