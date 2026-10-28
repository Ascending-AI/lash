//! Input-only JSON byte counting, independent of the concrete native writer.
use super::super::{DecimalLayout, Encoding, kind_bit, raise, text_copy};
use lash_kernel_doc::{Args, NativeError, Value, arena::ArenaHandle};

pub(super) struct Counter<'a, 'call> {
    pub(super) bytes: usize,
    pub(super) args: &'a Args<'call>,
    pub(super) layout: Option<DecimalLayout>,
    pub(super) verbatim_kinds: u8,
    pub(super) verbatim_field: Option<String>,
    pub(super) encoding: Option<Encoding>,
}
impl Counter<'_, '_> {
    fn push(&mut self, bytes: usize) -> Result<(), NativeError> {
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(crate::exhausted)?;
        Ok(())
    }
    fn quote(&mut self, text: &str) -> Result<(), NativeError> {
        self.push(2)?;
        let ascii = self.encoding.as_ref().is_some_and(|p| p.ascii);
        for c in text.chars() {
            self.push(match c {
                '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
                c if (c as u32) < 32 => 6,
                c if ascii && !(' '..='~').contains(&c) => 6 * c.len_utf16(),
                c => c.len_utf8(),
            })?;
        }
        Ok(())
    }
    fn separator(&mut self, item: bool) -> Result<(), NativeError> {
        self.push(self.encoding.as_ref().map_or(1, |p| {
            if item {
                p.item_separator.len()
            } else {
                p.key_separator.len()
            }
        }))
    }
    fn raw_numbers(&self, kind: &str, inherited: bool) -> bool {
        inherited && self.verbatim_kinds & kind_bit(kind) == 0
    }
    fn decimal(
        &mut self,
        layout: DecimalLayout,
        negative: bool,
        digits: &str,
        exponent: i64,
    ) -> Result<(), NativeError> {
        if negative && (digits != "0" || layout.negative_zero) {
            self.push(1)?;
        }
        let point = i64::try_from(digits.len())
            .map_err(|_| crate::exhausted())?
            .saturating_add(exponent);
        if point < layout.fixed_from || point > layout.fixed_until {
            self.push(digits.len() + usize::from(digits.len() > 1) + 1)?;
            let exponent = point - 1;
            self.push(usize::from(exponent < 0 || layout.exponent_plus))?;
            let width = self.encoding.as_ref().map_or(0, |p| p.exponent_digits);
            self.push(width.max(exponent.unsigned_abs().to_string().len()))
        } else if point <= 0 {
            let zeros = usize::try_from(-point).map_err(|_| crate::exhausted())?;
            self.push(
                2_usize
                    .checked_add(zeros)
                    .and_then(|n| n.checked_add(digits.len()))
                    .ok_or_else(crate::exhausted)?,
            )
        } else {
            let point = usize::try_from(point).map_err(|_| crate::exhausted())?;
            if point >= digits.len() {
                self.push(
                    point
                        + if self.encoding.as_ref().is_some_and(|p| p.fractional) {
                            2
                        } else {
                            0
                        },
                )
            } else {
                self.push(digits.len() + 1)
            }
        }
    }
    fn integer_layout(&mut self, layout: DecimalLayout, text: &str) -> Result<(), NativeError> {
        let digits = text.strip_prefix('-').unwrap_or(text);
        let trimmed = digits.trim_end_matches('0');
        let (digits, exponent) = if trimmed.is_empty() {
            ("0", 0)
        } else {
            (
                trimmed,
                i64::try_from(digits.len() - trimmed.len()).map_err(|_| crate::exhausted())?,
            )
        };
        self.decimal(layout, text.starts_with('-'), digits, exponent)
    }
    fn number(&mut self, value: &Value, selected: bool) -> Result<(), NativeError> {
        let layout = self
            .layout
            .filter(|_| selected && (self.encoding.is_none() || matches!(value, Value::Float(_))));
        if let Some(layout) = layout {
            match value {
                Value::Float(number) => {
                    let n = number.get();
                    if n.trunc() == n && n.abs() < 9007199254740992.0 {
                        let text = (n.abs() as u64).to_string();
                        let trimmed = text.trim_end_matches('0');
                        let (digits, exponent) = if trimmed.is_empty() {
                            ("0", 0)
                        } else {
                            (trimmed, (text.len() - trimmed.len()) as i64)
                        };
                        return self.decimal(layout, n.is_sign_negative(), digits, exponent);
                    }
                    let mut scratch = [0; 17];
                    let (negative, digits, exponent) = number
                        .decimal_parts(&mut scratch)
                        .ok_or_else(|| raise("json_number", "JSON refuses NaN and infinity"))?;
                    return self.decimal(layout, negative, digits, i64::from(exponent));
                }
                Value::Int(n) => return self.integer_layout(layout, &n.to_string()),
                Value::BigInt(_) => {
                    let n = self
                        .args
                        .integer(value)?
                        .ok_or(NativeError::Malformed("an integer names another body"))?;
                    return self.integer_layout(layout, &n.to_string());
                }
                _ => unreachable!("JSON number selection"),
            }
        }
        match value {
            Value::Int(n) => self.push(n.to_string().len()),
            Value::BigInt(_) => {
                let n = self
                    .args
                    .integer(value)?
                    .ok_or(NativeError::Malformed("an integer names another body"))?;
                self.push(n.to_string().len())
            }
            Value::Float(n) => self.push(n.to_string().len()),
            _ => unreachable!("JSON number selection"),
        }
    }
}

pub(super) fn write(
    value: &Value,
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Counter<'_, '_>,
) -> Result<(), NativeError> {
    let limit = out.args.limits().document_depth().min(64);
    if depth > limit {
        return Err(raise(
            "json_depth",
            format!("JSON exceeds the configured {limit}-level kernel nesting limit"),
        ));
    }
    let args = out.args;
    match value {
        Value::Null => out.push(4)?,
        Value::Bool(b) => out.push(if *b { 4 } else { 5 })?,
        Value::Int(_) | Value::BigInt(_) => out.number(value, raw_numbers)?,
        Value::Float(n) if n.get().is_finite() => out.number(value, raw_numbers)?,
        Value::Text(_) | Value::LongText(_) => out.quote(&text_copy(args, value)?)?,
        Value::Tuple(_) => {
            let raw = out.raw_numbers("tuple", raw_numbers);
            let items = super::super::super::sequence_values(args, value)?;
            array(&items, active, depth, raw, out)?;
        }
        Value::List(id) | Value::Map(id) | Value::Record(id) => {
            if active.contains(id) {
                return Err(raise("cycle", "cyclic value cannot be JSON"));
            }
            active.push(*id);
            if matches!(value, Value::List(_)) {
                let raw = out.raw_numbers("list", raw_numbers);
                let items = super::super::super::sequence_values(args, value)?;
                array(&items, active, depth, raw, out)?;
            } else {
                let table = args
                    .table(value)?
                    .ok_or(NativeError::Malformed("a table names another body"))?;
                let mut fields = vec![];
                for chunk in table.chunks() {
                    for pair in chunk?.as_chunks::<2>().0 {
                        if !matches!(pair[0], Value::Text(_) | Value::LongText(_)) {
                            return Err(raise("json_key", "JSON map keys must be text"));
                        }
                        if matches!(value, Value::Record(_))
                            && let Some(policy) = &out.encoding
                        {
                            let key = text_copy(args, &pair[0])?;
                            if policy.excluded.iter().any(|field| *key == **field) {
                                return Err(raise("not_data", "record has an excluded field"));
                            }
                        }
                        fields.push((&pair[0], &pair[1]));
                    }
                }
                let raw = if matches!(value, Value::Map(_)) {
                    out.raw_numbers("map", raw_numbers)
                } else {
                    let mut verbatim = false;
                    if let Some(field) = &out.verbatim_field {
                        for (key, value) in &fields {
                            if matches!(value, Value::Text(_) | Value::LongText(_))
                                && *text_copy(args, key)? == **field
                            {
                                verbatim = true;
                                break;
                            }
                        }
                    }
                    out.raw_numbers("record", raw_numbers) && !verbatim
                };
                out.push(1)?;
                for (at, (key, value)) in fields.iter().enumerate() {
                    if at > 0 {
                        out.separator(true)?;
                    }
                    out.quote(&text_copy(args, key)?)?;
                    out.separator(false)?;
                    write(value, active, depth + 1, raw, out)?;
                }
                out.push(1)?;
            }
            active.pop();
        }
        Value::Float(_) => return Err(raise("json_number", "JSON refuses NaN and infinity")),
        _ => return Err(raise("not_data", "value kind has no JSON representation")),
    }
    Ok(())
}
fn array(
    items: &[Value],
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw: bool,
    out: &mut Counter<'_, '_>,
) -> Result<(), NativeError> {
    out.push(1)?;
    for (at, value) in items.iter().enumerate() {
        if at > 0 {
            out.separator(true)?;
        }
        write(value, active, depth + 1, raw, out)?;
    }
    out.push(1)
}
