//! json.encode native version 1: the six-field style writer that archived
//! helper definitions bind. ADR 0149 keeps each Rust implementation while a
//! supported archived declaration binds it, so this is the exact pre-FIG-7637
//! answer and resource contract. Version 2 is the shared encoder; this module
//! must not change behavior.
use std::sync::Arc;

use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::{
    LogicalHeldV1 as Cost, NativeResourceContract as Contract, NativeResourceExit as Exit,
    NativeResourceOrigin as Origin,
};

use super::*;

/// The version 1 native, with its own call and resource contract.
pub(crate) fn native() -> Arc<dyn NativeFunction> {
    Arc::new(super::super::Native {
        function: encode,
        arity: 3..=3,
        prepared: None,
        text_append: false,
        resources: Some(contract),
    })
}

/// Reads the layout and six-field style; `held` reads each text argument.
fn style<'call>(
    args: &Args<'call>,
    mut held: impl FnMut(&'call Value) -> Result<Cow<'call, str>, NativeError>,
) -> Result<(DecimalLayout, Encoding), NativeError> {
    let layout = DecimalLayout::read(arg(args.values(), 1)?, args)?;
    let style = arg(args.values(), 2)?;
    let (Value::Tuple(_), Some(style)) = (style, args.sequence(style)?) else {
        return Err(raise("type_error", "expected JSON style tuple"));
    };
    if style.len() != 6 {
        return Err(raise("type_error", "expected six JSON style fields"));
    }
    let values = (0..6)
        .map(|index| style.get(index))
        .collect::<Result<Vec<_>, _>>()?;
    let [
        Some(Value::Bool(ascii)),
        Some(item),
        Some(key),
        Some(Value::Int(width)),
        Some(Value::Bool(fractional)),
        Some(excluded),
    ] = values.as_slice()
    else {
        return Err(raise("type_error", "invalid JSON style fields"));
    };
    if !(0..=20).contains(width) {
        return Err(raise("number_range", "exponent width must be in 0..20"));
    }
    let (Value::Tuple(_), Some(excluded)) = (excluded, args.sequence(excluded)?) else {
        return Err(raise("type_error", "expected excluded record fields"));
    };
    if excluded.len() != 3 {
        return Err(raise("type_error", "expected three excluded record fields"));
    }
    let excluded = (0..3)
        .map(|index| {
            let field = excluded
                .get(index)?
                .ok_or_else(|| raise("type_error", "missing excluded field"))?;
            Ok(held(field)?.into_owned())
        })
        .collect::<Result<Vec<_>, NativeError>>()?;
    let encoding = Encoding {
        ascii: *ascii,
        item_separator: held(item)?.into_owned(),
        key_separator: held(key)?.into_owned(),
        exponent_digits: *width as usize,
        fractional: *fractional,
        excluded,
        nonfinite: false,
        skipkeys: false,
    };
    Ok((layout, encoding))
}

fn encode(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let args = call.args;
    let (layout, encoding) = style(&args, |value| text_held(&args, call.new, value))?;
    let mut out = Out {
        output: TextOutput {
            text: String::new(),
            new: call.new,
        },
        args: Some(&args),
        layout: Some(layout),
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: Some(encoding),
        indentation: None,
    };
    write(
        &args,
        arg(args.values(), 0)?,
        &mut Vec::new(),
        0,
        true,
        &mut out,
    )?;
    let text = out.output.text;
    Ok(call.new.text(&text))
}

fn contract(args: Args<'_>) -> Result<Contract, NativeError> {
    let mut out = Out {
        output: output::Count::default(),
        args: Some(&args),
        layout: None,
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: None,
        indentation: None,
    };
    let answer = style(&args, |value| text_copy(&args, value)).and_then(|(layout, encoding)| {
        out.layout = Some(layout);
        out.encoding = Some(encoding);
        write(
            &args,
            arg(args.values(), 0)?,
            &mut vec![],
            0,
            true,
            &mut out,
        )
    });
    let mut contract = match answer {
        Ok(()) => Contract::value(
            Exit::Returned,
            Cost::REFERENCE + out.output.bytes as u64,
            Origin::Immutable,
        ),
        Err(error) => resources::refused(error)?,
    };
    let mut bytes = out.output.bytes as u64;
    for value in args.values() {
        bytes = bytes
            .checked_add(resources::workspace(
                args,
                value,
                &mut vec![],
                0,
                args.limits().document_depth().min(64),
            )?)
            .ok_or(NativeError::Malformed("native logical cost overflow"))?;
    }
    crate::text_json::resources::scratch(&mut contract, bytes)?;
    Ok(contract)
}

fn number<S: Output>(
    args: &Args<'_>,
    out: &mut Out<'_, '_, S>,
    value: &Value,
    selected: bool,
) -> Result<(), NativeError> {
    if let Some(layout) = out
        .layout
        .filter(|_| selected && (out.encoding.is_none() || matches!(value, Value::Float(_))))
    {
        match value {
            Value::Float(number) => {
                let value = number.get();
                if value.trunc() == value && value.abs() < 9007199254740992.0 {
                    return layout.small_integer(value.abs() as u64, value.is_sign_negative(), out);
                }
                let mut scratch = [0; 17];
                let (negative, digits, exponent) = number
                    .decimal_parts(&mut scratch)
                    .ok_or_else(|| raise("json_number", "JSON refuses NaN and infinity"))?;
                return layout.write(negative, digits, i64::from(exponent), out);
            }
            Value::Int(value) => {
                return layout.small_integer(value.unsigned_abs(), *value < 0, out);
            }
            Value::BigInt(_) => {
                let Some(number) = args.integer(value)? else {
                    return Err(NativeError::Malformed("an integer names another body"));
                };
                out.output.reserve(
                    number
                        .bits()
                        .div_ceil(8)
                        .saturating_mul(3)
                        .saturating_add(1),
                )?;
                let text = out.integer_digits(&number)?;
                #[cfg(test)]
                let _digits = crate::native_contract_laws::scratch(text.len() as u64);
                let digits = text.strip_prefix('-').unwrap_or(&text);
                let trimmed = digits.trim_end_matches('0');
                let (digits, exponent) = if trimmed.is_empty() {
                    ("0", 0)
                } else {
                    (
                        trimmed,
                        i64::try_from(digits.len() - trimmed.len())
                            .map_err(|_| crate::exhausted())?,
                    )
                };
                return layout.write(text.starts_with('-'), digits, exponent, out);
            }
            _ => unreachable!("the JSON writer selects only numbers"),
        }
    }
    match value {
        Value::Int(number) => match crate::numbers::short_digits(*number, 10) {
            Some(digits) => out.push(digits.as_str()),
            None => out.push(&number.to_string()),
        },
        Value::BigInt(_) => {
            let Some(number) = args.integer(value)? else {
                return Err(NativeError::Malformed("an integer names another body"));
            };
            let text = out.integer_digits(&number)?;
            #[cfg(test)]
            let _digits = crate::native_contract_laws::scratch(text.len() as u64);
            out.push(&text)
        }
        Value::Float(number) => out.push(&number.to_string()),
        _ => unreachable!("the JSON writer selects only numbers"),
    }
}

fn write<'call, S: Output>(
    args: &Args<'call>,
    value: &Value,
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_, '_, S>,
) -> Result<(), NativeError> {
    let limit = args.limits().document_depth().min(64);
    if depth > limit {
        return Err(raise(
            "json_depth",
            format!("JSON exceeds the configured {limit}-level kernel nesting limit"),
        ));
    }
    #[cfg(test)]
    let _frame = crate::native_contract_laws::scratch(4 * 16);
    match value {
        Value::Null => out.push("null")?,
        Value::Bool(value) => out.push(if *value { "true" } else { "false" })?,
        Value::Int(_) | Value::BigInt(_) => number(args, out, value, raw_numbers)?,
        Value::Float(float) if float.get().is_finite() => {
            number(args, out, value, raw_numbers)?;
        }
        Value::Text(_) | Value::LongText(_) => {
            let Some(text) = args.text(value)? else {
                return Err(NativeError::Malformed("a text names another body"));
            };
            out.output.read_whole(text.len())?;
            out.quote(&text.whole()?)?;
        }
        Value::Tuple(_) => {
            let raw_numbers = out.raw_numbers("tuple", raw_numbers);
            let items = crate::text_json::sequence_values(args, value)?;
            write_array(args, &items, active, depth, raw_numbers, out)?;
        }
        Value::List(id) | Value::Map(id) | Value::Record(id) => {
            if active.contains(id) {
                return Err(raise("cycle", "cyclic value cannot be JSON"));
            }
            active.push(*id);
            if matches!(value, Value::List(_)) {
                let raw_numbers = out.raw_numbers("list", raw_numbers);
                let items = crate::text_json::sequence_values(args, value)?;
                write_array(args, &items, active, depth, raw_numbers, out)?;
            } else {
                let Some(table) = args.table(value)? else {
                    return Err(NativeError::Malformed("a table names another body"));
                };
                let mut fields = Vec::with_capacity(table.len() as usize);
                for chunk in table.chunks() {
                    for pair in chunk?.as_chunks::<2>().0.iter() {
                        if !matches!(pair[0], Value::Text(_) | Value::LongText(_)) {
                            return Err(raise("json_key", "JSON map keys must be text"));
                        }
                        if matches!(value, Value::Record(_)) && out.encoding.is_some() {
                            let key = out.text_held(args, &pair[0])?;
                            if out.encoding.as_ref().is_some_and(|policy| {
                                policy.excluded.iter().any(|field| *key == **field)
                            }) {
                                return Err(raise("not_data", "record has an excluded field"));
                            }
                        }
                        fields.push((&pair[0], &pair[1]));
                    }
                }
                let raw_numbers = if matches!(value, Value::Map(_)) {
                    out.raw_numbers("map", raw_numbers)
                } else {
                    out.raw_numbers("record", raw_numbers)
                };
                #[cfg(test)]
                let _fields = crate::native_contract_laws::scratch_values(
                    *args,
                    &fields
                        .iter()
                        .flat_map(|(a, b)| [**a, **b])
                        .collect::<Vec<_>>(),
                );
                out.push("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        out.separator(true)?;
                    }
                    let name = out.text_held(args, name)?;
                    out.quote(&name)?;
                    out.separator(false)?;
                    write(args, value, active, depth + 1, raw_numbers, out)?;
                }
                out.push("}")?;
            }
            active.pop();
        }
        Value::Float(_) => return Err(raise("json_number", "JSON refuses NaN and infinity")),
        _ => return Err(raise("not_data", "value kind has no JSON representation")),
    }
    Ok(())
}

fn write_array<S: Output>(
    args: &Args<'_>,
    items: &[Value],
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_, '_, S>,
) -> Result<(), NativeError> {
    #[cfg(test)]
    let _items = crate::native_contract_laws::scratch_values(*args, items);
    out.push("[")?;
    for (index, value) in items.iter().enumerate() {
        if index != 0 {
            out.separator(true)?;
        }
        write(args, value, active, depth + 1, raw_numbers, out)?;
    }
    out.push("]")
}
