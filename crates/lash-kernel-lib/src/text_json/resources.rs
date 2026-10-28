//! Input-only logical contracts. Payload sizes come from semantic summaries;
//! neither paged read copies nor nursery room are guest scratch obligations.
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
        _ => return None,
    })
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
    text_result(
        text(args, 0)?
            .len()
            .checked_add(text(args, 1)?.len())
            .ok_or_else(overflow)?,
    )
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
    text_result(payload.checked_add(separators).ok_or_else(overflow)?)
}
