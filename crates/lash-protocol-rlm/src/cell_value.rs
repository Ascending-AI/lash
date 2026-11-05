//! Guest JSON projections share the host/text encoder. A renderer must always
//! produce a diagnostic, so a refused document becomes one error record carrying
//! the encoder's call and path. It never recursively invents JSON for guest kinds.
use lash_kernel_doc::{Datum, Float, Integer, NumberPolicy};
use serde_json::Value as Json;

pub(crate) fn datum_json(value: &Datum, model: lash_kernel_lib::JsonModel, context: &str) -> Json {
    lash_kernel_lib::encode_datum(
        value,
        model,
        lash_kernel_lib::JsonTarget::Text,
        context,
        lash_kernel_value::data_limits::KernelDataLimits::standard().document_depth(),
    )
    .map_err(|error| error.to_string())
    .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
    .unwrap_or_else(|message| serde_json::json!({"$kind": "not_data", "message": message}))
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

    /// Guest diagnostics use the same omission and path rules as JSON text.
    #[test]
    fn every_datum_has_a_json_reading() {
        let value = Datum::Record(vec![
            ("missing".into(), Datum::Absent),
            (
                "items".into(),
                Datum::List(vec![Datum::Absent, Datum::Null]),
            ),
        ]);
        assert_eq!(
            datum_json(&value, lash_kernel_lib::JsonModel::JavaScript, "print"),
            serde_json::json!({"items": [null, null]})
        );
        let invalid = Datum::Record(vec![(
            "bad".into(),
            Datum::Bytes(lash_kernel_doc::Bytes::new(vec![])),
        )]);
        let error = datum_json(&invalid, lash_kernel_lib::JsonModel::JavaScript, "print");
        assert!(error["message"].as_str().unwrap().contains("print $.bad"));
    }

    /// A seeded value reads back as the JSON it was seeded from.
    #[test]
    fn seeded_json_reads_back() {
        let seeded = serde_json::json!({"rows": [1, 2.5, "x"], "ok": true});
        let datum = json_datum(&seeded, NumberPolicy::BySpelling);
        assert_eq!(
            datum_json(
                &datum,
                lash_kernel_lib::JsonModel::Kernel,
                "session.binding"
            ),
            seeded
        );
    }
}
