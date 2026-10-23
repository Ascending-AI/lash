//! Total text leaves: Unicode 15 membership and Python's quoted text.

use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};

use crate::plans::{NONPRINTABLE_RANGES, WHITESPACE_RANGES};

#[derive(Clone, Copy)]
pub(super) enum Predicate {
    Printable,
    Whitespace,
}

fn contains(ranges: &[(i64, i64)], point: i64) -> (bool, u64) {
    let mut probes = 0;
    let found = ranges
        .binary_search_by(|(low, high)| {
            probes += 1;
            if *high < point {
                std::cmp::Ordering::Less
            } else if *low > point {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok();
    (found, probes)
}

impl Predicate {
    fn answer(self, point: i64) -> bool {
        self.counted(point).0
    }

    fn counted(self, point: i64) -> (bool, u64) {
        match self {
            Self::Whitespace => contains(WHITESPACE_RANGES, point),
            Self::Printable if point < 32 => (false, 1),
            Self::Printable if point <= 126 => (true, 2),
            Self::Printable => {
                let (found, probes) = contains(NONPRINTABLE_RANGES, point);
                (!found, probes + 2)
            }
        }
    }
}

fn printable(args: &[Value]) -> Option<Value> {
    let [Value::Int(point)] = args else {
        return None;
    };
    Some(Value::Bool(Predicate::Printable.answer(*point)))
}

fn whitespace(args: &[Value]) -> Option<Value> {
    let [Value::Int(point)] = args else {
        return None;
    };
    Some(Value::Bool(Predicate::Whitespace.answer(*point)))
}

impl NativeFunction for Predicate {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [point] = call.args.values() else {
            return Err(NativeError::Decline);
        };
        let Some(point) = call.args.integer(point)? else {
            return Err(NativeError::Decline);
        };
        let (answer, work) = match point.word() {
            Some(point) => self.counted(point),
            // Non-word integers lie outside every authored range. The
            // printable body's initial comparison still rejects negatives.
            None => (matches!(self, Self::Printable) && !point.is_negative(), 1),
        };
        call.counter.spend(work)?;
        Ok(Value::Bool(answer))
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: match self {
                Self::Printable => printable,
                Self::Whitespace => whitespace,
            },
            charged_entry: None,
            operation: match self {
                Self::Printable => "py.text.printable(point: Int) -> Bool",
                Self::Whitespace => "py.text.whitespace(point: Int) -> Bool",
            },
        })
    }
}

pub(super) struct Repr;

/// Count and write through the same scalar walk. The first pass reserves the
/// result before allocation, and a retry keeps no output or nursery objects.
fn quoted(text: &str, mut emit: impl FnMut(&str)) -> u64 {
    let mut work = 2 * text.len() as u64;
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut bytes = [0; 4];
    emit(quote.encode_utf8(&mut bytes));
    for character in text.chars() {
        work += character.len_utf8() as u64;
        let (printable, probes) = Predicate::Printable.counted(i64::from(u32::from(character)));
        work += probes;
        match character {
            '\\' => emit("\\\\"),
            '\t' => emit("\\t"),
            '\n' => emit("\\n"),
            '\r' => emit("\\r"),
            c if c == quote => {
                emit("\\");
                emit(c.encode_utf8(&mut bytes));
            }
            c if printable => emit(c.encode_utf8(&mut bytes)),
            c => {
                let point = u32::from(c);
                // All spellings fit this fixed buffer; no per-scalar allocation.
                let (prefix, width) = if point < 256 {
                    ("\\x", 2)
                } else if point < 65536 {
                    ("\\u", 4)
                } else {
                    ("\\U", 8)
                };
                emit(prefix);
                let mut digits = [b'0'; 8];
                for index in 0..width {
                    let digit = ((point >> (4 * index)) & 15) as usize;
                    digits[width - index - 1] = b"0123456789abcdef"[digit];
                }
                emit(
                    std::str::from_utf8(&digits[..width])
                        .unwrap_or_else(|_| unreachable!("hex digits are ASCII")),
                );
            }
        }
    }
    emit(quote.encode_utf8(&mut bytes));
    work
}

impl NativeFunction for Repr {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [value] = call.args.values() else {
            return Err(NativeError::Decline);
        };
        let Some(view) = call.args.text(value)? else {
            return Err(NativeError::Decline);
        };
        call.new.read_whole(view.len())?;
        let text = view.whole()?;
        let mut length = 0_u64;
        let first_work = quoted(&text, |part| {
            length = length.saturating_add(part.len() as u64)
        });
        // At most 16 input-byte units per pass: two quote-selection scans,
        // scalar reads, and up to eleven probes plus the two ASCII checks.
        // Both passes also count each output byte once (count, then write).
        call.new.reserve(0, length.saturating_mul(2))?;
        let mut output = String::new();
        output
            .try_reserve_exact(usize::try_from(length).unwrap_or(usize::MAX))
            .map_err(|_| {
                NativeError::Retry(lash_kernel_doc::Retry::Room {
                    wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
                })
            })?;
        let second_work = quoted(&text, |part| output.push_str(part));
        call.counter.spend(
            8_u64
                .saturating_add(first_work)
                .saturating_add(second_work)
                .saturating_add(length.saturating_mul(2)),
        )?;
        Ok(call.new.text(&output))
    }
}
