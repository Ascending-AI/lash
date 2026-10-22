//! Inline conversion answers; the helper bodies own every allocating case.

use std::fmt::Write as _;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};

pub(super) struct ToBoolean;

fn to_boolean(args: &[Value]) -> Option<Value> {
    let [value] = args else { return None };
    Some(Value::Bool(match value {
        Value::Null | Value::Absent => false,
        Value::Bool(value) => *value,
        Value::Int(value) => *value != 0,
        Value::Float(value) => value.get() != 0.0 && !value.get().is_nan(),
        Value::Text(value) => !value.as_str().is_empty(),
        // Long texts are nonempty and non-word integers are nonzero by
        // the arena's canonical representation. Every object is truthy.
        _ => true,
    }))
}

impl NativeFunction for ToBoolean {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        to_boolean(call.args.values()).ok_or(NativeError::Decline)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: to_boolean,
            charged_entry: None,
            operation: "ts.to_boolean(value: Any) -> Bool",
        })
    }
}

/// A fixed inline buffer: failure leaves allocation and its admission to
/// the existing helper body, rather than building a speculative long text.
#[derive(Default)]
struct Text {
    bytes: [u8; ShortText::MAX],
    len: usize,
}

impl std::fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(std::fmt::Error)?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or(std::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl Text {
    fn value(&self) -> Option<Value> {
        ShortText::ascii(&self.bytes[..self.len]).map(Value::Text)
    }
}

pub(super) struct NumberToString;

/// Decimal word digits without the generic formatting protocol. Either the
/// whole spelling fits inline, or the helper body handles its allocation.
fn integer_text(word: i64) -> Option<Value> {
    let mut bytes = [0u8; ShortText::MAX];
    let mut at = bytes.len();
    let mut rest = word.unsigned_abs();
    loop {
        at = at.checked_sub(1)?;
        bytes[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    if word < 0 {
        at = at.checked_sub(1)?;
        bytes[at] = b'-';
    }
    ShortText::ascii(&bytes[at..]).map(Value::Text)
}

fn number_to_string(args: &[Value]) -> Option<Value> {
    let [value] = args else { return None };
    let mut output = Text::default();
    match value {
        Value::Int(word) => return integer_text(*word),
        Value::Float(number) => {
            let value = number.get();
            // This is the body's first arm, including either signed zero.
            if value.abs() < 9007199254740992.0 && value.floor() == value {
                return integer_text(value as i64);
            } else if value.is_nan() {
                output.write_str("NaN").ok()?;
            } else if value.is_infinite() {
                output
                    .write_str(if value.is_sign_negative() {
                        "-Infinity"
                    } else {
                        "Infinity"
                    })
                    .ok()?;
            } else {
                // Reuse format.decimal_parts' shortest converter and its tie
                // choice, then apply ts.number.layout's exact thresholds.
                let mut scratch = [0; 17];
                let (negative, digits, exponent) = number.decimal_parts(&mut scratch)?;
                let point = i32::try_from(digits.len()).ok()? + exponent;
                if negative {
                    output.write_char('-').ok()?;
                }
                if !(-5..=21).contains(&point) {
                    output.write_str(digits.get(..1)?).ok()?;
                    if digits.len() > 1 {
                        output.write_char('.').ok()?;
                        output.write_str(digits.get(1..)?).ok()?;
                    }
                    let exponent = point - 1;
                    write!(
                        output,
                        "e{}{exponent}",
                        if exponent >= 0 { "+" } else { "" }
                    )
                    .ok()?;
                } else if point <= 0 {
                    output.write_str("0.").ok()?;
                    for _ in point..0 {
                        output.write_char('0').ok()?;
                    }
                    output.write_str(digits).ok()?;
                } else {
                    let point = usize::try_from(point).ok()?;
                    if point >= digits.len() {
                        output.write_str(digits).ok()?;
                        for _ in digits.len()..point {
                            output.write_char('0').ok()?;
                        }
                    } else {
                        output.write_str(digits.get(..point)?).ok()?;
                        output.write_char('.').ok()?;
                        output.write_str(digits.get(point..)?).ok()?;
                    }
                }
            }
        }
        _ => return None,
    }
    output.value()
}

impl NativeFunction for NumberToString {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        number_to_string(call.args.values()).ok_or(NativeError::Decline)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: number_to_string,
            charged_entry: None,
            operation: "ts.number.format_value(value: Number) -> Text",
        })
    }
}
