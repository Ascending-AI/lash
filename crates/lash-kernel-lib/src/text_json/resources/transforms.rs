//! Payload facts for text and byte operations, without guest allocation.
use super::*;
use crate::text_json::{arg, count_arg, integer_value, raise, sequence, text_arg};
use num_traits::ToPrimitive;
use unicode_normalization::UnicodeNormalization;

fn scalar(args: Args<'_>, value: Value) -> Result<NativeResourceContract, NativeError> {
    NativeResourceContract::returned(args, &value)
}
fn boolean(args: Args<'_>, flag: bool) -> Result<NativeResourceContract, NativeError> {
    scalar(args, Value::Bool(flag))
}
fn number(args: Args<'_>, n: i64) -> Result<NativeResourceContract, NativeError> {
    scalar(args, Value::Int(n))
}
fn list(items: Vec<lash_kernel_doc::NativeResourceHolding>) -> NativeResourceContract {
    use lash_kernel_doc::{
        NativeResourceEntity as Entity, NativeResourceEvent as Event,
        NativeResourceHolding as Holding, NativeResourceOwner as Owner,
    };
    NativeResourceContract {
        exit: NativeResourceExit::Returned,
        output: Holding {
            bytes: LogicalHeldV1::REFERENCE,
            origin: NativeResourceOrigin::Fresh(Owner(0)),
        },
        events: vec![Event::Entity {
            owner: Owner(0),
            kind: Entity::List,
            release: None,
            members: items,
        }],
    }
}
fn member(bytes: u64) -> lash_kernel_doc::NativeResourceHolding {
    lash_kernel_doc::NativeResourceHolding {
        bytes: LogicalHeldV1::REFERENCE + bytes,
        origin: NativeResourceOrigin::Immutable,
    }
}
fn output(bytes: usize) -> Result<NativeResourceContract, NativeError> {
    let mut contract = text_result(bytes as u64)?;
    scratch(&mut contract, bytes as u64)?;
    Ok(contract)
}
fn pad_bytes(text: &str, width: usize, fill: &str) -> Result<usize, NativeError> {
    let needed = width.saturating_sub(text.chars().count());
    if needed == 0 || fill.is_empty() {
        return Ok(text.len());
    }
    let chars = fill.chars().count();
    (needed / chars)
        .checked_mul(fill.len())
        .and_then(|n| {
            n.checked_add(
                fill.chars()
                    .take(needed % chars)
                    .map(char::len_utf8)
                    .sum::<usize>(),
            )
        })
        .and_then(|n| n.checked_add(text.len()))
        .ok_or_else(crate::exhausted)
}
pub(super) fn text_op(args: Args<'_>, op: &str) -> Result<NativeResourceContract, NativeError> {
    if op == "utf16_compare" {
        for at in 0..2 {
            if args.text(arg(args.values(), at)?)?.is_none() {
                return Err(raise("type_error", "expected text keys"));
            }
        }
    }
    let a = text_arg(&args, 0)?;
    match op {
        "compare" | "utf16_compare" => {
            let b = text_arg(&args, 1)?;
            let order = if op == "compare" {
                a.cmp(&b)
            } else {
                a.encode_utf16().cmp(b.encode_utf16())
            };
            number(
                args,
                match order {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                },
            )
        }
        "starts_with" => boolean(args, a.starts_with(text_arg(&args, 1)?.as_ref())),
        "ends_with" => boolean(args, a.ends_with(text_arg(&args, 1)?.as_ref())),
        "get" | "utf16_get" => {
            let index = integer_value(&args, 1)?;
            let length = if op == "get" {
                a.chars().count()
            } else {
                a.encode_utf16().count()
            };
            let distance = index
                .to_i128()
                .and_then(|n| usize::try_from(n.unsigned_abs()).ok())
                .unwrap_or(usize::MAX);
            let at = if index.is_negative() {
                length.checked_sub(distance)
            } else {
                Some(distance)
            }
            .filter(|at| *at < length)
            .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))?;
            if op == "get" {
                output(a.chars().nth(at).ok_or_else(overflow)?.len_utf8())
            } else {
                number(
                    args,
                    i64::from(a.encode_utf16().nth(at).ok_or_else(overflow)?),
                )
            }
        }
        "slice" | "utf16_slice" => {
            let length = if op == "slice" {
                a.chars().count()
            } else {
                a.encode_utf16().count()
            };
            let start = crate::text_json::text::position(&*integer_value(&args, 1)?, length);
            let end =
                crate::text_json::text::position(&*integer_value(&args, 2)?, length).max(start);
            let mut units = 0;
            let mut start_byte = None;
            let mut end_byte = None;
            for (at, c) in a.char_indices().chain(std::iter::once((a.len(), '\0'))) {
                if units == start {
                    start_byte = Some(at);
                }
                if units == end {
                    end_byte = Some(at);
                }
                units += if op == "slice" { 1 } else { c.len_utf16() };
            }
            let (Some(start), Some(end)) = (start_byte, end_byte) else {
                return Err(raise(
                    "text_boundary",
                    "UTF-16 slice splits a surrogate pair",
                ));
            };
            output(end - start)
        }
        "find" | "utf16_find" => {
            let needle = text_arg(&args, 1)?;
            let start = integer_value(&args, 2)?;
            let count = |s: &str| {
                if op == "find" {
                    s.chars().count()
                } else {
                    s.encode_utf16().count()
                }
            };
            let length = count(&a);
            let target = crate::text_json::text::position(&start, length);
            if needle.is_empty() {
                return number(args, target as i64);
            }
            let mut units = 0;
            for (at, c) in a.char_indices() {
                if units >= target && a[at..].starts_with(needle.as_ref()) {
                    return number(args, units as i64);
                }
                units += if op == "find" { 1 } else { c.len_utf16() };
            }
            number(args, -1)
        }
        "split" => {
            let b = text_arg(&args, 1)?;
            let limit = if matches!(args.get(2), None | Some(Value::Absent)) {
                usize::MAX
            } else {
                count_arg(&args, 2)?
            };
            let strings: Vec<_> = if b.is_empty() {
                a.char_indices()
                    .map(|(at, c)| &a[at..at + c.len_utf8()])
                    .take(limit)
                    .collect()
            } else {
                a.split(b.as_ref()).take(limit).collect()
            };
            Ok(list(
                strings
                    .into_iter()
                    .map(|s| member(s.len() as u64))
                    .collect(),
            ))
        }
        "replace" => {
            let b = text_arg(&args, 1)?;
            let c = text_arg(&args, 2)?;
            let n = a.matches(b.as_ref()).count();
            output(
                n.checked_mul(c.len())
                    .and_then(|n| n.checked_add(a.len() - a.matches(b.as_ref()).count() * b.len()))
                    .ok_or_else(crate::exhausted)?,
            )
        }
        "repeat" => output(
            a.len()
                .checked_mul(count_arg(&args, 1)?)
                .ok_or_else(crate::exhausted)?,
        ),
        "pad_start" | "pad_end" => {
            output(pad_bytes(&a, count_arg(&args, 1)?, &text_arg(&args, 2)?)?)
        }
        "trim_set" => {
            let b = text_arg(&args, 1)?;
            let (Value::Bool(leading), Value::Bool(trailing)) =
                (arg(args.values(), 2)?, arg(args.values(), 3)?)
            else {
                return Err(raise("type_error", "expected boolean"));
            };
            let a = if *leading {
                a.trim_start_matches(|c| b.contains(c))
            } else {
                &a
            };
            let a = if *trailing {
                a.trim_end_matches(|c| b.contains(c))
            } else {
                a
            };
            output(a.len())
        }
        "trim" => output(a.trim().len()),
        "trim_start" => output(a.trim_start().len()),
        "trim_end" => output(a.trim_end().len()),
        "lower" => output(a.to_lowercase().len()),
        "upper" => output(a.to_uppercase().len()),
        "normalize" => {
            let form = text_arg(&args, 1)?;
            let bytes = match form.as_ref() {
                "NFC" => a.nfc().map(char::len_utf8).sum(),
                "NFD" => a.nfd().map(char::len_utf8).sum(),
                "NFKC" => a.nfkc().map(char::len_utf8).sum(),
                "NFKD" => a.nfkd().map(char::len_utf8).sum(),
                _ => {
                    return Err(raise(
                        "normalization_form",
                        "expected NFC, NFD, NFKC or NFKD",
                    ));
                }
            };
            let mut contract = output(bytes)?;
            // Unicode's maximum recursive decomposition is 18 scalars.
            scratch(
                &mut contract,
                (a.chars().count() as u64)
                    .checked_mul(18 * LogicalHeldV1::REFERENCE)
                    .ok_or_else(overflow)?,
            )?;
            Ok(contract)
        }
        "to_code_points" | "to_utf16_units" => {
            let ints: Vec<u32> = if op == "to_code_points" {
                a.chars().map(u32::from).collect()
            } else {
                a.encode_utf16().map(u32::from).collect()
            };
            Ok(list(
                ints.into_iter()
                    .map(|n| member(u64::from(u32::BITS - n.leading_zeros()).div_ceil(8)))
                    .collect(),
            ))
        }
        _ => Err(NativeError::Malformed("unknown text resource operation")),
    }
}
pub(super) fn from_units(
    args: Args<'_>,
    utf16: bool,
) -> Result<NativeResourceContract, NativeError> {
    let items = sequence(&args, 0)?;
    let mut contract = if utf16 {
        let units = items
            .iter()
            .map(|v| match v {
                Value::Int(n) => u16::try_from(*n)
                    .map_err(|_| raise("invalid_utf16", "UTF-16 unit is outside 0..65535")),
                Value::BigInt(_) => Err(raise("invalid_utf16", "UTF-16 unit is outside 0..65535")),
                _ => Err(raise("type_error", "expected integer UTF-16 unit")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        output(
            String::from_utf16(&units)
                .map_err(|_| raise("invalid_utf16", "UTF-16 contains an unpaired surrogate"))?
                .len(),
        )?
    } else {
        let mut bytes = 0_usize;
        for v in &items {
            let n = match v {
                Value::Int(n) => u32::try_from(*n).ok(),
                Value::BigInt(_) => None,
                _ => return Err(raise("type_error", "expected integer code point")),
            };
            let c = n
                .and_then(char::from_u32)
                .ok_or_else(|| raise("invalid_scalar", "code point is not a Unicode scalar"))?;
            bytes = bytes.checked_add(c.len_utf8()).ok_or_else(overflow)?;
        }
        output(bytes)?
    };
    let mut workspace = 0_u64;
    for value in &items {
        workspace = workspace
            .checked_add(LogicalHeldV1::native_local(args, value)?)
            .ok_or_else(overflow)?;
    }
    if utf16 {
        workspace = workspace
            .checked_add(items.len() as u64 * LogicalHeldV1::REFERENCE)
            .ok_or_else(overflow)?;
    }
    scratch(&mut contract, workspace)?;
    Ok(contract)
}
pub(super) fn bytes_op(args: Args<'_>, op: &str) -> Result<NativeResourceContract, NativeError> {
    let get = |at| {
        args.bytes(arg(args.values(), at)?)
            .and_then(|v| v.ok_or_else(|| raise("type_error", "expected bytes")))
    };
    match op {
        "from_octets" => {
            let items = sequence(&args, 0)?;
            for value in &items {
                match value {
                    Value::Int(n) if (0..=255).contains(n) => {}
                    Value::Int(_) | Value::BigInt(_) => {
                        return Err(raise("number_range", "octet is outside 0..255"));
                    }
                    _ => return Err(raise("type_error", "expected integer octet")),
                }
            }
            output(items.len())
        }
        "utf8_encode" => output(text_arg(&args, 0)?.len()),
        "utf8_decode" => {
            let bytes = get(0)?.whole()?;
            output(
                std::str::from_utf8(&bytes)
                    .map_err(|_| raise("invalid_utf8", "bytes are not well-formed UTF-8"))?
                    .len(),
            )
        }
        "concat" => output(
            get(0)?
                .len()
                .checked_add(get(1)?.len())
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(overflow)?,
        ),
        "slice" => {
            let bytes = get(0)?;
            let len = bytes.len() as usize;
            let start = crate::text_json::text::position(&*integer_value(&args, 1)?, len);
            let end = crate::text_json::text::position(&*integer_value(&args, 2)?, len).max(start);
            output(end - start)
        }
        "compare" => number(
            args,
            match get(0)?.whole()?.cmp(&get(1)?.whole()?) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            },
        ),
        "at" => {
            let bytes = get(0)?;
            let at = integer_value(&args, 1)?
                .word()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (*n as u64) < bytes.len())
                .ok_or_else(|| raise("index", "byte index is outside the byte string"))?;
            number(args, i64::from(bytes.whole()?[at]))
        }
        _ => Err(NativeError::Malformed("unknown bytes resource operation")),
    }
}
