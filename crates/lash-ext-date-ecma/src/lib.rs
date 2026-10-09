//! Deterministic ECMAScript ISO date parsing and UTC formatting.
//!
//! A valid date is a kernel timestamp. Invalid dates are the branded record
//! `{brand: "date.invalid"}`. Natives never mutate their arguments; calendar
//! arithmetic and JavaScript coercion live in the dialect's kernel helpers.
mod calendar;

use lash_kernel_doc::{
    ErrorValue, Float, FunctionId, FunctionRegistry, Integer, NativeCall, NativeError,
    NativeFunction, Object, RegistryError, Timestamp, Value, parse_definition,
};
use num_traits::ToPrimitive;
use std::{collections::BTreeMap, sync::Arc};

/// Registers deterministic date conversion, ISO parsing and formatting functions.
#[expect(
    clippy::expect_used,
    reason = "constant library definitions are valid kernel text"
)]
pub fn register(
    registry: &mut FunctionRegistry,
) -> Result<BTreeMap<String, FunctionId>, RegistryError> {
    let mut result = BTreeMap::new();
    for (name, signature, op) in [
        (
            "date.ecma.timestamp",
            "(milliseconds: Float) -> Any",
            Op::Timestamp,
        ),
        (
            "date.ecma.milliseconds",
            "(date: Any) -> Float",
            Op::Milliseconds,
        ),
        ("date.ecma.parse", "(source: Text) -> Float", Op::Parse),
        (
            "date.ecma.format",
            "(date: Any, format: Text) -> Text",
            Op::Format,
        ),
    ] {
        let text = format!(
            "function {name}{signature}\nkernel 1\nerrors \"type_error\", \"RangeError\", \"TS_DATE_PARSE_NON_ISO\"\ncharge sum(32, deep(milliseconds), deep(date), size(source), size(format), size(result))\nnative\n"
        );
        // Each formula references only its own arguments.
        let charge = match op {
            Op::Timestamp => "sum(32, size(milliseconds), deep(result))",
            Op::Milliseconds => "sum(32, deep(date))",
            Op::Parse => "sum(32, size(source))",
            Op::Format => "sum(32, deep(date), size(format), size(result))",
        };
        let text = text.replace(
            "sum(32, deep(milliseconds), deep(date), size(source), size(format), size(result))",
            charge,
        );
        let definition = parse_definition(&text).expect("date extension definition");
        result.insert(
            name.to_owned(),
            registry.register(definition, Some(Arc::new(op)))?,
        );
    }
    Ok(result)
}
#[derive(Clone, Copy)]
enum Op {
    Timestamp,
    Milliseconds,
    Parse,
    Format,
}
fn raise(kind: &str, message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new(kind, message))
}
fn milliseconds(value: &Value, call: &NativeCall<'_>) -> Result<f64, NativeError> {
    match value {
        Value::Timestamp(time) => (time.nanoseconds.as_bigint() / 1_000_000_i64)
            .to_f64()
            .ok_or_else(|| raise("RangeError", "Timestamp out of range")),
        Value::Record(id)
            if call.heap.record_get(*id, "brand") == Some(Value::text("date.invalid")) =>
        {
            Ok(f64::NAN)
        }
        _ => Err(raise("type_error", "Date receiver required")),
    }
}
impl NativeFunction for Op {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let first = call
            .args
            .first()
            .ok_or_else(|| raise("type_error", "Missing argument"))?;
        match self {
            Self::Timestamp => {
                let Value::Float(time) = first else {
                    return Err(raise("type_error", "Expected float"));
                };
                let clipped = calendar::time_clip(time.get());
                if clipped.is_nan() {
                    return call
                        .heap
                        .allocate(Object::Record(vec![(
                            "brand".into(),
                            Value::text("date.invalid"),
                        )]))
                        .map(Value::Record);
                }
                let integer = Integer::parse(&format!("{clipped:.0}"))
                    .map_err(|_| raise("RangeError", "Invalid time"))?;
                Ok(Value::Timestamp(Timestamp {
                    nanoseconds: Integer::new(integer.as_bigint() * 1_000_000),
                }))
            }
            Self::Milliseconds => Ok(Value::Float(Float::new(milliseconds(first, &call)?))),
            Self::Parse => {
                let Value::Text(source) = first else {
                    return Err(raise("type_error", "Expected text"));
                };
                match calendar::parse_iso_date(source) {
                    Ok(time) => Ok(Value::Float(Float::new(time))),
                    Err(calendar::IsoDateError::Invalid) => Ok(Value::Float(Float::new(f64::NAN))),
                    Err(calendar::IsoDateError::NonIso) => Err(raise(
                        "TS_DATE_PARSE_NON_ISO",
                        "Use the ECMA ISO date-time format",
                    )),
                }
            }
            Self::Format => {
                let time = milliseconds(first, &call)?;
                let Some(Value::Text(format)) = call.args.get(1) else {
                    return Err(raise("type_error", "Expected format"));
                };
                let text = match format.as_ref() {
                    "iso" => calendar::to_iso_string(time)
                        .ok_or_else(|| raise("RangeError", "Invalid time value"))?,
                    "utc" => calendar::date_utc_string(time),
                    "string" => calendar::date_to_string(time),
                    _ => return Err(raise("type_error", "Unknown date format")),
                };
                Ok(Value::text(text))
            }
        }
    }
}
