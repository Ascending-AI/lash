use lash_kernel_doc::{Bytes, NativeCall, NativeError, Type, Value};
use num_traits::ToPrimitive;

use super::text::position;
use super::{
    Function, arg, definition, integer_value, ordering, raise, sequence, sequence_type, text_arg,
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

fn bytes_arg(args: &[Value], index: usize) -> Result<&[u8], NativeError> {
    match arg(args, index)? {
        Value::Bytes(bytes) => Ok(bytes.as_slice()),
        _ => Err(raise("type_error", "expected bytes")),
    }
}

fn from_octets(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let bytes: Result<Vec<_>, _> = sequence(&call, 0)?
        .iter()
        .map(|value| {
            let Value::Int(i) = value else {
                return Err(raise("type_error", "expected integer octet"));
            };
            i.to_u8()
                .ok_or_else(|| raise("number_range", "octet is outside 0..255"))
        })
        .collect();
    Ok(Value::Bytes(Bytes::new(bytes?)))
}

fn slice(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let bytes = bytes_arg(call.args, 0)?;
    let start = position(integer_value(call.args, 1)?, bytes.len());
    let end = position(integer_value(call.args, 2)?, bytes.len());
    Ok(Value::Bytes(Bytes::new(if start > end {
        Vec::new()
    } else {
        bytes[start..end].to_vec()
    })))
}

fn concat(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::Bytes(Bytes::new(
        [bytes_arg(call.args, 0)?, bytes_arg(call.args, 1)?].concat(),
    )))
}

fn compare(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(ordering(
        bytes_arg(call.args, 0)?.cmp(bytes_arg(call.args, 1)?),
    ))
}

fn encode(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::Bytes(Bytes::new(
        text_arg(call.args, 0)?.as_bytes().to_vec(),
    )))
}

fn decode(call: NativeCall<'_>) -> Result<Value, NativeError> {
    std::str::from_utf8(bytes_arg(call.args, 0)?)
        .map(Value::text)
        .map_err(|_| raise("invalid_utf8", "bytes are not well-formed UTF-8"))
}
