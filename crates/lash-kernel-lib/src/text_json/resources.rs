//! Input-only logical contracts. Payload sizes come from semantic summaries;
//! neither paged read copies nor nursery room are guest scratch obligations.
mod formatting;
mod transforms;

use lash_kernel_doc::{
    Args, LogicalHeldV1, NativeError, NativeResourceContract, NativeResourceExit,
    NativeResourceOrigin, Value,
};

pub(super) type Evaluator = fn(Args<'_>) -> Result<NativeResourceContract, NativeError>;

pub(super) fn evaluator(name: &str) -> Option<Evaluator> {
    Some(match name {
        "text.join" => |args| join(args, false),
        "text.join_or_index" => |args| join(args, true),
        "text.concat" => concat,
        "text.len" => |args| text_length(args, false),
        "text.utf16_len" => |args| text_length(args, true),
        "text.get" => |args| attempt(args, |args| transforms::text_op(args, "get")),
        "text.utf16_get" => |args| attempt(args, |args| transforms::text_op(args, "utf16_get")),
        "text.slice" => |args| attempt(args, |args| transforms::text_op(args, "slice")),
        "text.utf16_slice" => |args| attempt(args, |args| transforms::text_op(args, "utf16_slice")),
        "text.find" => |args| attempt(args, |args| transforms::text_op(args, "find")),
        "text.utf16_find" => |args| attempt(args, |args| transforms::text_op(args, "utf16_find")),
        "text.compare" => |args| attempt(args, |args| transforms::text_op(args, "compare")),
        "text.utf16_compare" => {
            |args| attempt(args, |args| transforms::text_op(args, "utf16_compare"))
        }
        "text.starts_with" => |args| attempt(args, |args| transforms::text_op(args, "starts_with")),
        "text.ends_with" => |args| attempt(args, |args| transforms::text_op(args, "ends_with")),
        "text.split" => |args| attempt(args, |args| transforms::text_op(args, "split")),
        "text.replace" => |args| attempt(args, |args| transforms::text_op(args, "replace")),
        "text.repeat" => |args| attempt(args, |args| transforms::text_op(args, "repeat")),
        "text.pad_start" => |args| attempt(args, |args| transforms::text_op(args, "pad_start")),
        "text.pad_end" => |args| attempt(args, |args| transforms::text_op(args, "pad_end")),
        "text.trim_set" => |args| attempt(args, |args| transforms::text_op(args, "trim_set")),
        "text.to_code_points" => {
            |args| attempt(args, |args| transforms::text_op(args, "to_code_points"))
        }
        "text.to_utf16_units" => {
            |args| attempt(args, |args| transforms::text_op(args, "to_utf16_units"))
        }
        "text.from_code_points" => |args| attempt(args, |args| transforms::from_units(args, false)),
        "text.from_utf16_units" => |args| attempt(args, |args| transforms::from_units(args, true)),
        "bytes.at" => |args| attempt(args, |args| transforms::bytes_op(args, "at")),
        "bytes.from_octets" => {
            |args| attempt(args, |args| transforms::bytes_op(args, "from_octets"))
        }
        "bytes.slice" => |args| attempt(args, |args| transforms::bytes_op(args, "slice")),
        "bytes.concat" => |args| attempt(args, |args| transforms::bytes_op(args, "concat")),
        "bytes.compare" => |args| attempt(args, |args| transforms::bytes_op(args, "compare")),
        "bytes.utf8_encode" => {
            |args| attempt(args, |args| transforms::bytes_op(args, "utf8_encode"))
        }
        "bytes.utf8_decode" => {
            |args| attempt(args, |args| transforms::bytes_op(args, "utf8_decode"))
        }
        "format.decimal_parts" => {
            |args| attempt(args, |args| formatting::evaluate(args, "decimal_parts"))
        }
        "format.fixed" => |args| attempt(args, |args| formatting::evaluate(args, "fixed")),
        "format.scientific" => {
            |args| attempt(args, |args| formatting::evaluate(args, "scientific"))
        }
        "format.radix" => |args| attempt(args, |args| formatting::evaluate(args, "radix")),
        "format.pad" => |args| attempt(args, |args| formatting::evaluate(args, "pad")),
        "json.parse" => |args| super::json::resources::evaluate(args, "parse"),
        "json.parse_binary64" => |args| super::json::resources::evaluate(args, "parse_binary64"),
        "json.decode" => |args| super::json::resources::evaluate(args, "decode"),
        "json.stringify" => |args| super::json::resources::evaluate(args, "stringify"),
        "json.render" => |args| super::json::resources::evaluate(args, "render"),
        "json.encode" => |args| super::json::resources::evaluate(args, "encode"),
        "json.classify" => {
            |args| NativeResourceContract::reading(args, super::classify::classify_value(args))
        }
        "timestamp.nanoseconds" => |args| timestamp(args, false),
        "timestamp.from_nanoseconds" => |args| timestamp(args, true),
        name if name.starts_with("text.trim_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "trim"))
        }
        name if name.starts_with("text.trim_start_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "trim_start"))
        }
        name if name.starts_with("text.trim_end_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "trim_end"))
        }
        name if name.starts_with("text.lower_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "lower"))
        }
        name if name.starts_with("text.upper_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "upper"))
        }
        name if name.starts_with("text.normalize_u") => {
            |args| attempt(args, |args| transforms::text_op(args, "normalize"))
        }
        _ => return None,
    })
}

pub(super) use crate::resource_contracts::scratch;

fn attempt(
    args: Args<'_>,
    evaluate: impl FnOnce(Args<'_>) -> Result<NativeResourceContract, NativeError>,
) -> Result<NativeResourceContract, NativeError> {
    // The list snapshot and its scalar conversion workspace are independent
    // of whether a later member refuses. Keep them through that raised exit.
    let mut workspace = 0_u64;
    if let Some(first) = args.get(0)
        && matches!(first, Value::List(_))
    {
        for chunk in args.sequence(first)?.ok_or_else(overflow)?.chunks() {
            for value in chunk? {
                workspace = workspace
                    .checked_add(LogicalHeldV1::native_local(args, value)?)
                    .and_then(|n| n.checked_add(LogicalHeldV1::REFERENCE))
                    .ok_or_else(overflow)?;
            }
        }
    }
    let mut contract = match evaluate(args) {
        Ok(contract) => contract,
        Err(NativeError::Refused { kind, message }) => {
            NativeResourceContract::refused(kind, &message).ok_or_else(overflow)?
        }
        Err(error) => return Err(error),
    };
    scratch(&mut contract, workspace)?;
    Ok(contract)
}
fn timestamp(args: Args<'_>, from: bool) -> Result<NativeResourceContract, NativeError> {
    let value = super::arg(args.values(), 0)?;
    let integer = if from {
        args.integer(value)?
    } else {
        args.timestamp(value)?
    }
    .ok_or_else(|| {
        super::raise(
            "type_error",
            if from {
                "expected integer"
            } else {
                "expected timestamp"
            },
        )
    })?;
    Ok(NativeResourceContract::value(
        NativeResourceExit::Returned,
        LogicalHeldV1::REFERENCE + integer.bits().div_ceil(8),
        NativeResourceOrigin::Immutable,
    ))
}

fn overflow() -> NativeError {
    NativeError::Malformed("native logical cost overflow")
}

fn text(args: Args<'_>, at: usize) -> Result<lash_kernel_doc::TextView<'_>, NativeError> {
    let value = args
        .get(at)
        .ok_or_else(|| super::raise("arity", "missing argument"))?;
    args.text(value)?
        .ok_or_else(|| super::raise("type_error", "expected text"))
}

fn text_result(bytes: u64) -> Result<NativeResourceContract, NativeError> {
    Ok(NativeResourceContract::value(
        NativeResourceExit::Returned,
        LogicalHeldV1::REFERENCE
            .checked_add(bytes)
            .ok_or_else(overflow)?,
        NativeResourceOrigin::Immutable,
    ))
}

fn text_length(args: Args<'_>, utf16: bool) -> Result<NativeResourceContract, NativeError> {
    let summary = text(args, 0)?.summary();
    let length = if utf16 {
        summary.utf16_units
    } else {
        summary.chars
    };
    NativeResourceContract::returned(args, &Value::Int(length as i64))
}

fn concat(args: Args<'_>) -> Result<NativeResourceContract, NativeError> {
    let payload = text(args, 0)?
        .len()
        .checked_add(text(args, 1)?.len())
        .ok_or_else(overflow)?;
    let mut contract = text_result(payload)?;
    scratch(&mut contract, payload)?;
    Ok(contract)
}

fn join(args: Args<'_>, index: bool) -> Result<NativeResourceContract, NativeError> {
    let value = args
        .get(0)
        .ok_or_else(|| super::raise("arity", "missing argument"))?;
    let (Value::List(_), Some(sequence)) = (value, args.sequence(value)?) else {
        return Err(super::raise("type_error", "expected list"));
    };
    let mut payload = 0_u64;
    let mut count = 0_u64;
    for chunk in sequence.chunks() {
        for member in chunk? {
            let Some(text) = args.text(member)? else {
                return if index {
                    NativeResourceContract::returned(args, &Value::Int(count as i64))
                } else {
                    Err(super::raise("type_error", "join requires text members"))
                };
            };
            payload = payload.checked_add(text.len()).ok_or_else(overflow)?;
            count = count.checked_add(1).ok_or_else(overflow)?;
        }
    }
    let separators = text(args, 1)?
        .len()
        .checked_mul(count.saturating_sub(1))
        .ok_or_else(overflow)?;
    let payload = payload.checked_add(separators).ok_or_else(overflow)?;
    let mut contract = text_result(payload)?;
    scratch(&mut contract, payload)?;
    Ok(contract)
}
