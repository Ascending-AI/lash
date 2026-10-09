use lash_kernel_doc::{NativeCall, NativeError, Type, Value};
use num_traits::ToPrimitive;

use super::{Function, arg, count_arg, definition, integer_arg, raise, text_arg};

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

fn precision(call: &NativeCall<'_>) -> Result<usize, NativeError> {
    // Precision is a count, never a dialect-specific default. Avoid the
    // formatter's infallible allocation for sizes outside String's domain.
    let precision = count_arg(call, 1)?;
    let mut probe = String::new();
    probe
        .try_reserve(precision.checked_add(400).ok_or(NativeError::Memory)?)
        .map_err(|_| NativeError::Memory)?;
    Ok(precision)
}

fn fixed(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let precision = precision(&call)?;
    let text = match arg(&call, 0)? {
        Value::Int(i) => {
            let whole = i.to_string();
            if precision == 0 {
                whole
            } else {
                format!("{whole}.{}", "0".repeat(precision))
            }
        }
        Value::Float(f) => {
            if !f.get().is_finite() {
                return Err(raise(
                    "number_range",
                    "precision formatting requires a finite number",
                ));
            }
            format!("{:.*}", precision, f.get())
        }
        _ => return Err(raise("type_error", "expected number")),
    };
    Ok(Value::text(text))
}

fn scientific(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Value::Float(f) = arg(&call, 0)? else {
        return Err(raise("type_error", "expected float"));
    };
    if !f.get().is_finite() {
        return Err(raise(
            "number_range",
            "precision formatting requires a finite number",
        ));
    }
    Ok(Value::text(format!("{:.*e}", precision(&call)?, f.get())))
}

fn radix(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let radix = integer_arg(&call, 1)?
        .to_u32()
        .filter(|r| (2..=36).contains(r))
        .ok_or_else(|| raise("number_range", "radix must be between 2 and 36"))?;
    Ok(Value::text(integer_arg(&call, 0)?.to_str_radix(radix)))
}

fn pad(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let start = match text_arg(&call, 3)? {
        "start" => true,
        "end" => false,
        _ => return Err(raise("type_error", "padding side must be start or end")),
    };
    super::text::padded(
        text_arg(&call, 0)?,
        count_arg(&call, 1)?,
        text_arg(&call, 2)?,
        start,
    )
}
