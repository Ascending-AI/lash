use std::fmt::Write as _;

use lash_kernel_doc::{NativeCall, NativeError, NativeHeap, Type, Value};
use num_traits::ToPrimitive;

use super::{Function, arg, count_arg, definition, integer_arg, raise, text_arg, text_buffer};

pub(super) fn functions() -> Vec<Function> {
    vec![
        definition(
            "format.fixed",
            &[("number", Type::Number), ("precision", Type::Int)],
            Type::Text,
            &[],
            fixed,
        ),
        definition(
            "format.scientific",
            &[("number", Type::Float), ("precision", Type::Int)],
            Type::Text,
            &[],
            scientific,
        ),
        definition(
            "format.radix",
            &[("integer", Type::Int), ("radix", Type::Int)],
            Type::Text,
            &[],
            radix,
        ),
        definition(
            "format.pad",
            &[
                ("text", Type::Text),
                ("width", Type::Int),
                ("fill", Type::Text),
                ("side", Type::Enum(vec!["start".into(), "end".into()])),
            ],
            Type::Text,
            &[],
            pad,
        ),
    ]
}

/// A float's digits before the point, its sign and its point: what a
/// fixed rendering holds besides its fraction digits.
const FIXED_WHOLE: usize = 400;
/// A scientific rendering besides its fraction digits: sign, leading digit,
/// point and exponent.
const SCIENTIFIC_FRAME: usize = 32;

fn fixed(call: NativeCall<'_>) -> Result<Value, NativeError> {
    // Precision is a count, never a dialect-specific default. The digits it
    // asks for are reserved before the formatter writes one.
    let precision = count_arg(call.args, 1)?;
    let text = match arg(call.args, 0)? {
        Value::Int(i) => {
            let whole = i.to_string();
            if precision == 0 {
                whole
            } else {
                let size = precision
                    .checked_add(whole.len() + 1)
                    .ok_or(NativeError::Memory)?;
                let mut out = text_buffer(call.heap, size)?;
                out.push_str(&whole);
                out.push('.');
                out.extend(std::iter::repeat_n('0', precision));
                out
            }
        }
        Value::Float(f) => {
            if !f.get().is_finite() {
                return Err(raise(
                    "number_range",
                    "precision formatting requires a finite number",
                ));
            }
            precise(call.heap, precision, FIXED_WHOLE, |out| {
                write!(out, "{:.*}", precision, f.get())
            })?
        }
        _ => return Err(raise("type_error", "expected number")),
    };
    Ok(Value::text(text))
}

fn scientific(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Value::Float(f) = arg(call.args, 0)? else {
        return Err(raise("type_error", "expected float"));
    };
    if !f.get().is_finite() {
        return Err(raise(
            "number_range",
            "precision formatting requires a finite number",
        ));
    }
    let precision = count_arg(call.args, 1)?;
    precise(call.heap, precision, SCIENTIFIC_FRAME, |out| {
        write!(out, "{:.*e}", precision, f.get())
    })
    .map(Value::text)
}

/// Writes a float to `precision` fraction digits into a buffer reserved for
/// them and for the `frame` around them.
fn precise(
    heap: &mut dyn NativeHeap,
    precision: usize,
    frame: usize,
    write: impl FnOnce(&mut String) -> std::fmt::Result,
) -> Result<String, NativeError> {
    let size = precision.checked_add(frame).ok_or(NativeError::Memory)?;
    let mut out = text_buffer(heap, size)?;
    // Writing to a `String` does not fail.
    write(&mut out).map_err(|_| NativeError::Memory)?;
    Ok(out)
}

fn radix(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let radix = integer_arg(call.args, 1)?
        .to_u32()
        .filter(|r| (2..=36).contains(r))
        .ok_or_else(|| raise("number_range", "radix must be between 2 and 36"))?;
    Ok(Value::text(integer_arg(call.args, 0)?.to_str_radix(radix)))
}

fn pad(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let start = match text_arg(call.args, 3)? {
        "start" => true,
        "end" => false,
        _ => return Err(raise("type_error", "padding side must be start or end")),
    };
    super::text::padded(
        call.heap,
        text_arg(call.args, 0)?,
        count_arg(call.args, 1)?,
        text_arg(call.args, 2)?,
        start,
    )
}
