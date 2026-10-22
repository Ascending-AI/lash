use super::Selection;
use std::borrow::Cow;

use lash_kernel_doc::{Args, BytesView, NativeCall, NativeError, Value};

use lash_vocabulary::Type;

use super::text::position;
use super::{
    Function, arg, definition, integer_value, materialized_text, ordering, raise, room, sequence,
    sequence_type,
};
pub(super) fn functions(selection: &Selection<'_>) -> Vec<Function> {
    vec![
        definition(
            selection,
            "bytes.at",
            &[("bytes", Type::Bytes), ("index", Type::Int)],
            Type::Int,
            &["index"],
            at,
        ),
        definition(
            selection,
            "bytes.from_octets",
            &[("items", sequence_type(Type::Int))],
            Type::Bytes,
            &[],
            from_octets,
        ),
        definition(
            selection,
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
            selection,
            "bytes.concat",
            &[("bytes", Type::Bytes), ("other", Type::Bytes)],
            Type::Bytes,
            &[],
            concat,
        ),
        definition(
            selection,
            "bytes.compare",
            &[("bytes", Type::Bytes), ("other", Type::Bytes)],
            Type::Int,
            &[],
            compare,
        ),
        definition(
            selection,
            "bytes.utf8_encode",
            &[("text", Type::Text)],
            Type::Bytes,
            &[],
            encode,
        ),
        definition(
            selection,
            "bytes.utf8_decode",
            &[("bytes", Type::Bytes)],
            Type::Text,
            &["invalid_utf8"],
            decode,
        ),
    ]
}

fn bytes_view<'call>(args: &Args<'call>, index: usize) -> Result<BytesView<'call>, NativeError> {
    args.bytes(arg(args.values(), index)?)?
        .ok_or_else(|| raise("type_error", "expected bytes"))
}
fn bytes_arg<'call>(args: &Args<'call>, index: usize) -> Result<Cow<'call, [u8]>, NativeError> {
    bytes_view(args, index)?.whole()
}
fn from_octets(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let bytes: Result<Vec<_>, _> = sequence(&call.args, 0)?
        .iter()
        .map(|value| {
            match value {
                Value::Int(i) => u8::try_from(*i).ok(),
                Value::BigInt(_) => None,
                _ => return Err(raise("type_error", "expected integer octet")),
            }
            .ok_or_else(|| raise("number_range", "octet is outside 0..255"))
        })
        .collect();
    Ok(call.new.bytes(&bytes?))
}

fn slice(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let bytes = bytes_view(&call.args, 0)?;
    let len = bytes.len() as usize;
    let start = position(&*integer_value(&call.args, 1)?, len);
    let end = position(&*integer_value(&call.args, 2)?, len).max(start);
    call.new.hold(0, room(end - start))?;
    Ok(call.new.bytes(&bytes.whole()?[start..end]))
}
fn concat(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let (bytes, other) = (bytes_arg(&call.args, 0)?, bytes_arg(&call.args, 1)?);
    call.new
        .hold(0, room(bytes.len()).saturating_add(room(other.len())))?;
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len() + other.len())
        .map_err(|_| crate::exhausted())?;
    out.extend_from_slice(&bytes);
    out.extend_from_slice(&other);
    Ok(call.new.bytes(&out))
}
fn compare(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    Ok(ordering(
        bytes_arg(&call.args, 0)?.cmp(&bytes_arg(&call.args, 1)?),
    ))
}
fn encode(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    Ok(call.new.bytes(text.as_bytes()))
}
fn decode(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let bytes = bytes_view(&call.args, 0)?;
    call.new.read_whole(bytes.len())?;
    let bytes = bytes.whole()?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| raise("invalid_utf8", "bytes are not well-formed UTF-8"))?;
    Ok(call.new.text(text))
}

/// A pure byte read. The default deep-size charge bounds traversal across
/// resident chunks as well as materializing cold input.
fn at(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let bytes = bytes_view(&call.args, 0)?;
    let index = integer_value(&call.args, 1)?
        .word()
        .and_then(|word| u64::try_from(word).ok())
        .ok_or_else(|| raise("index", "byte index is outside the byte string"))?;
    if index >= bytes.len() {
        return Err(raise("index", "byte index is outside the byte string"));
    }
    let mut remaining = index;
    for chunk in bytes.chunks() {
        let chunk = chunk?;
        if remaining < chunk.len() as u64 {
            return Ok(Value::Int(i64::from(chunk[remaining as usize])));
        }
        remaining -= chunk.len() as u64;
    }
    Err(raise("index", "byte index is outside the byte string"))
}
