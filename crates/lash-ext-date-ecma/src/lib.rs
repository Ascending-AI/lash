//! Deterministic ECMAScript ISO date parsing and UTC formatting.
//!
//! A valid date is a kernel timestamp. Invalid dates are the branded record
//! `{brand: "date.invalid"}`. Natives never mutate their arguments; calendar
//! arithmetic and JavaScript coercion live in the dialect's kernel helpers.
mod calendar;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::{
    Args, Float, FunctionId, FunctionRegistry, Integer, NativeCall, NativeError, NativeFunction,
    NewObject, RegistryError, Value, parse_definition,
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
    for (signature, op) in [
        ("(milliseconds: Float) -> Any", Operation::Timestamp),
        ("(date: Any) -> Float", Operation::Milliseconds),
        ("(source: Text) -> Float", Operation::Parse),
        ("(date: Any, format: Text) -> Text", Operation::Format),
    ] {
        let name = op.name();
        let text = format!(
            "function {name}{signature}\nkernel 1\nerrors \"type_error\", \"RangeError\", \"TS_DATE_PARSE_NON_ISO\"\ncharge sum(32, deep(milliseconds), deep(date), size(source), size(format), size(result))\nnative\n"
        );
        // Each formula references only its own arguments.
        let charge = match op {
            Operation::Timestamp => "sum(32, size(milliseconds), deep(result))",
            Operation::Milliseconds => "sum(32, deep(date))",
            Operation::Parse => "sum(32, size(source))",
            Operation::Format => "sum(32, deep(date), size(format), size(result))",
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Timestamp,
    Milliseconds,
    Parse,
    Format,
}
impl Operation {
    pub const ALL: [Self; 4] = [
        Self::Timestamp,
        Self::Milliseconds,
        Self::Parse,
        Self::Format,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Timestamp => "date.ecma.timestamp",
            Self::Milliseconds => "date.ecma.milliseconds",
            Self::Parse => "date.ecma.parse",
            Self::Format => "date.ecma.format",
        }
    }
}

fn raise(kind: &'static str, message: &'static str) -> NativeError {
    NativeError::refused(kind, message)
}
fn milliseconds(value: &Value, args: &Args<'_>) -> Result<f64, NativeError> {
    match value {
        Value::Timestamp(_) | Value::BigTimestamp(_) => {
            let Some(nanoseconds) = args.timestamp(value)? else {
                return Err(raise("type_error", "Date receiver required"));
            };
            (&*nanoseconds.as_bigint() / 1_000_000_i64)
                .to_f64()
                .ok_or_else(|| raise("RangeError", "Timestamp out of range"))
        }
        Value::Record(_) => {
            let Some(fields) = args.table(value)? else {
                return Err(NativeError::Malformed("a record names another body"));
            };
            let brand = args.lookup(fields, &short("brand"))?;
            if let Some(brand) = brand
                && args.equal(brand, &short("date.invalid"))?
            {
                Ok(f64::NAN)
            } else {
                Err(raise("type_error", "Date receiver required"))
            }
        }
        _ => Err(raise("type_error", "Date receiver required")),
    }
}
impl NativeFunction for Operation {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        let first = args
            .get(0)
            .ok_or_else(|| raise("type_error", "Missing argument"))?;
        match self {
            Self::Timestamp => {
                let Value::Float(time) = first else {
                    return Err(raise("type_error", "Expected float"));
                };
                let clipped = calendar::time_clip(time.get());
                if clipped.is_nan() {
                    return Ok(call.new.allocate(NewObject::Record(vec![(
                        short("brand"),
                        short("date.invalid"),
                    )])));
                }
                let integer = Integer::parse(&format!("{clipped:.0}"))
                    .map_err(|_| raise("RangeError", "Invalid time"))?;
                Ok(call
                    .new
                    .timestamp(Integer::new(&*integer.as_bigint() * 1_000_000)))
            }
            Self::Milliseconds => Ok(Value::Float(Float::new(milliseconds(first, &args)?))),
            Self::Parse => {
                let Some(source) = args.text(first)? else {
                    return Err(raise("type_error", "Expected text"));
                };
                // A paged source is copied out of its pages.
                call.new.read_whole(source.len())?;
                match calendar::parse_iso_date(&source.whole()?) {
                    Ok(time) => Ok(Value::Float(Float::new(time))),
                    Err(calendar::IsoDateError::Invalid) => Ok(Value::Float(Float::new(f64::NAN))),
                    Err(calendar::IsoDateError::NonIso) => Err(raise(
                        "TS_DATE_PARSE_NON_ISO",
                        "Use the ECMA ISO date-time format",
                    )),
                }
            }
            Self::Format => {
                let time = milliseconds(first, &args)?;
                let format = match args.get(1) {
                    Some(format) => args.text(format)?,
                    None => None,
                };
                let Some(format) = format else {
                    return Err(raise("type_error", "Expected format"));
                };
                call.new.read_whole(format.len())?;
                let text = match format.whole()?.as_ref() {
                    "iso" => calendar::to_iso_string(time)
                        .ok_or_else(|| raise("RangeError", "Invalid time value"))?,
                    "utc" => calendar::date_utc_string(time),
                    "string" => calendar::date_to_string(time),
                    _ => return Err(raise("type_error", "Unknown date format")),
                };
                Ok(call.new.text(&text))
            }
        }
    }
}

/// A fixed name, inline.
fn short(name: &'static str) -> Value {
    match ShortText::new(name) {
        Some(name) => Value::Text(name),
        None => unreachable!("a fixed name fits an inline text"),
    }
}

/// Adds natives to the registered build definitions this extension implements.
pub fn implement(
    registry: &mut FunctionRegistry,
    functions: impl IntoIterator<Item = FunctionId>,
) -> Result<(), RegistryError> {
    for function in functions {
        let registered = registry
            .get(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        if let Some(operation) = Operation::ALL
            .into_iter()
            .find(|operation| operation.name() == registered.name())
        {
            registry.implement(function, Arc::new(operation))?;
        }
    }
    Ok(())
}
