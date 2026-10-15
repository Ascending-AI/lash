use lash_kernel_doc::heap::{BytesRef, IntRef, Payload};
use lash_kernel_doc::{NativeCall, NativeError, Type, Value};
use num_traits::ToPrimitive;
use std::sync::Arc;

use super::text::position;
use super::{
    Function, arg, definition, integer_value, materialized_text, ordering, raise, sequence,
    sequence_type,
};

pub(super) fn functions() -> Vec<Function> {
    vec![
        definition(
            "bytes.from_octets",
            &[("items", sequence_type(Type::Int))],
            Type::Bytes,
            &[],
            from_octets,
        ),
        definition(
            "bytes.slice",
            &[
                ("bytes", Type::Bytes),
                ("start", Type::Int),
                ("end", Type::Int),
            ],
            Type::Bytes,
            &[],
            slice,
        ),
        definition(
            "bytes.concat",
            &[("bytes", Type::Bytes), ("other", Type::Bytes)],
            Type::Bytes,
            &[],
            concat,
        ),
        definition(
            "bytes.compare",
            &[("bytes", Type::Bytes), ("other", Type::Bytes)],
            Type::Int,
            &[],
            compare,
        ),
        definition(
            "bytes.utf8_encode",
            &[("text", Type::Text)],
            Type::Bytes,
            &[],
            encode,
        ),
        definition(
            "bytes.utf8_decode",
            &[("bytes", Type::Bytes)],
            Type::Text,
            &["invalid_utf8"],
            decode,
        ),
    ]
}

fn bytes_arg(args: &[Value], index: usize) -> Result<&BytesRef, NativeError> {
    match arg(args, index)? {
        Value::Bytes(bytes) => Ok(bytes),
        _ => Err(raise("type_error", "expected bytes")),
    }
}
fn materialized_bytes(call: &mut NativeCall<'_>, index: usize) -> Result<Arc<[u8]>, NativeError> {
    let bytes = bytes_arg(call.args, index)?;
    match &bytes.body {
        Payload::Resident(bytes) => Ok(Arc::clone(bytes)),
        _ => call.heap.materialize_bytes(bytes),
    }
}
fn from_octets(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let bytes: Result<Vec<_>, _> = sequence(&call, 0)?
        .iter()
        .map(|value| {
            let Value::Int(i) = value else {
                return Err(raise("type_error", "expected integer octet"));
            };
            match i {
                IntRef::Inline(i) => i.to_u8(),
                _ => None,
            }
            .ok_or_else(|| raise("number_range", "octet is outside 0..255"))
        })
        .collect();
    Ok(Value::Bytes(call.heap.bytes(&bytes?)?))
}

fn slice(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let bytes = bytes_arg(call.args, 0)?;
    let start = position(integer_value(call.args, 1)?, bytes.len as usize) as u64;
    let end = (position(integer_value(call.args, 2)?, bytes.len as usize) as u64).max(start);
    call.heap.slice(&Value::Bytes(bytes.clone()), start..end)
}
fn concat(call: NativeCall<'_>) -> Result<Value, NativeError> {
    bytes_arg(call.args, 0)?;
    bytes_arg(call.args, 1)?;
    call.heap.concat(call.args)
}
fn compare(call: NativeCall<'_>) -> Result<Value, NativeError> {
    bytes_arg(call.args, 0)?;
    bytes_arg(call.args, 1)?;
    Ok(ordering(crate::comparison::payload_order(
        arg(call.args, 0)?,
        arg(call.args, 1)?,
        call.heap,
    )?))
}
fn encode(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = materialized_text(call.args, 0, call.heap)?;
    Ok(Value::Bytes(call.heap.bytes(text.as_bytes())?))
}
fn decode(mut call: NativeCall<'_>) -> Result<Value, NativeError> {
    let bytes = materialized_bytes(&mut call, 0)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| raise("invalid_utf8", "bytes are not well-formed UTF-8"))?;
    Ok(Value::Text(call.heap.text(text)?))
}
