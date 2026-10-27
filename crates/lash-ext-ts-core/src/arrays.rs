//! Shallow copies of plain arrays. Branded receivers stay with their bodies.

use lash_kernel_doc::{Args, NativeCall, NativeError, NativeFunction, NewObject, Retry, Value};

pub(super) struct Spread;

fn sequence<'a>(
    args: Args<'a>,
    value: &Value,
) -> Result<lash_kernel_doc::SequenceView<'a>, NativeError> {
    args.sequence(value)?.ok_or(NativeError::Decline)
}

fn room() -> NativeError {
    Retry::Room {
        wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
    }
    .into()
}

fn output(call: &mut NativeCall<'_, '_>, length: u64) -> Result<Vec<Value>, NativeError> {
    call.new.reserve(length, 0)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(usize::try_from(length).map_err(|_| room())?)
        .map_err(|_| room())?;
    Ok(values)
}

fn append(args: Args<'_>, source: &Value, values: &mut Vec<Value>) -> Result<(), NativeError> {
    if matches!(source, Value::List(_)) {
        for chunk in sequence(args, source)?.chunks() {
            values.extend_from_slice(chunk?);
        }
    } else {
        values.push(*source);
    }
    Ok(())
}

impl NativeFunction for Spread {
    fn call(&self, mut call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        let [parts] = args.values() else {
            return Err(NativeError::Decline);
        };
        if !matches!(parts, Value::List(_)) {
            return Err(NativeError::Decline);
        }
        let parts = sequence(args, parts)?;
        let mut count = 0_u64;
        for chunk in parts.chunks() {
            for part in chunk? {
                if !matches!(part, Value::List(_)) {
                    return Err(NativeError::Decline);
                }
                count = count
                    .checked_add(sequence(args, part)?.len())
                    .ok_or_else(room)?;
            }
        }
        let mut values = output(&mut call, count)?;
        for chunk in parts.chunks() {
            for part in chunk? {
                append(args, part, &mut values)?;
            }
        }
        Ok(call.new.allocate(NewObject::List(values)))
    }
}
