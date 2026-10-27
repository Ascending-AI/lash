//! Typed scalar kernels: the answers of the numeric operators on two
//! floats or two word integers, shared by the library's prepared entries,
//! whose own paths they are, and the machine's typed operator paths.
//! A kernel answers only where its answer is the
//! entry's; `None` leaves the call to the entry's general path, which
//! answers or declines.

use crate::{Float, Value};

/// The scalar charge table bit: a nonzero word has size 2, other
/// immediate scalar values have size 1. Heap values need measurement.
#[inline(always)]
pub fn size_bit(value: Value) -> Option<u8> {
    match value {
        Value::Int(word) => Some(word_size_bit(word)),
        Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_) => Some(0),
        _ => None,
    }
}

/// The size bit for a value already proven to be a word integer.
#[inline(always)]
pub fn word_size_bit(word: i64) -> u8 {
    u8::from(word != 0)
}

/// Guest equality for values whose answer needs no arena reads.
#[inline]
pub fn immediate_equal(a: &Value, b: &Value) -> Option<bool> {
    use Value as V;
    let numeric = |value: &Value| matches!(value, V::Int(_) | V::BigInt(_) | V::Float(_));
    Some(match (a, b) {
        (V::Null, V::Null) | (V::Absent, V::Absent) => true,
        (V::Bool(a), V::Bool(b)) => a == b,
        (V::Int(a), V::Int(b)) => a == b,
        (V::Float(a), V::Float(b)) => a.get() == b.get(),
        (V::Int(i), V::Float(f)) | (V::Float(f), V::Int(i)) => {
            // 2^63 is the first float past every word integer.
            const PAST: f64 = 9_223_372_036_854_775_808.0;
            let float = f.get();
            float.fract() == 0.0 && (-PAST..PAST).contains(&float) && float as i64 == *i
        }
        (V::Text(a), V::Text(b)) => a == b,
        (V::Text(_), V::LongText(_))
        | (V::LongText(_), V::Text(_))
        | (V::Int(_), V::BigInt(_))
        | (V::BigInt(_), V::Int(_)) => false,
        (V::Timestamp(a), V::Timestamp(b)) => a == b,
        (V::Task(a), V::Task(b)) | (V::TaskRef(a), V::TaskRef(b)) => a == b,
        (V::Closure(a), V::Closure(b)) | (V::ObjectRef(a), V::ObjectRef(b)) => a == b,
        _ if a.kind() != b.kind() && !(numeric(a) && numeric(b)) => false,
        _ => return None,
    })
}

/// A numeric operator a prepared entry answers two immediates by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarOp {
    Add,
    Sub,
    Mul,
    Div,
    RemTrunc,
    RemFloor,
    FloorDiv,
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Which kernel answers an entry's two floats and which its two word
/// integers ([`crate::PreparedNative::pair`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pair {
    pub floats: Option<ScalarOp>,
    pub ints: Option<ScalarOp>,
}

impl ScalarOp {
    /// Whether it answers a truth rather than a number.
    #[inline(always)]
    pub fn compares(self) -> bool {
        matches!(self, Self::Eq | Self::Lt | Self::Le | Self::Gt | Self::Ge)
    }
}

/// `op` on two floats.
#[inline(always)]
pub fn floats(op: ScalarOp, a: Float, b: Float) -> Option<Value> {
    float_answer(
        op,
        a.get(),
        b.get(),
        |number| Some(Value::Float(Float::new(number))),
        |truth| Some(Value::Bool(truth)),
    )
}

/// Arithmetic `op` on two floats, its answer not yet made canonical: a
/// NaN's payload is any NaN's, which no kernel tells apart, so a typed
/// block makes canonical only what it writes. `None` for a comparison.
#[inline(always)]
pub fn float_number(op: ScalarOp, a: f64, b: f64) -> Option<f64> {
    float_answer(op, a, b, Some, |_| None)
}

/// Comparison `op` on two floats; `None` for arithmetic.
#[inline(always)]
pub fn float_truth(op: ScalarOp, a: f64, b: f64) -> Option<bool> {
    float_answer(op, a, b, |_| None, Some)
}

/// `op`'s answer on two floats, as `number` or `truth` takes it: the one
/// match every float path answers by.
#[inline(always)]
fn float_answer<T>(
    op: ScalarOp,
    a: f64,
    b: f64,
    number: impl FnOnce(f64) -> Option<T>,
    truth: impl FnOnce(bool) -> Option<T>,
) -> Option<T> {
    match op {
        ScalarOp::Add => number(a + b),
        ScalarOp::Sub => number(a - b),
        ScalarOp::Mul => number(a * b),
        ScalarOp::Div => number(a / b),
        // `fmod` of integral operands below 2^63 in magnitude with a
        // nonzero divisor is their integer remainder, a zero keeping the
        // dividend's sign; every other pair is the general path's.
        ScalarOp::RemTrunc => {
            let (x, y) = (a as i64, b as i64);
            if !(x as f64 == a && y as f64 == b && x != i64::MAX && y != i64::MAX && y != 0) {
                return None;
            }
            number(match x.wrapping_rem(y) {
                0 => 0.0_f64.copysign(a),
                r => r as f64,
            })
        }
        ScalarOp::RemFloor | ScalarOp::FloorDiv => None,
        ScalarOp::Eq => truth(a == b),
        ScalarOp::Lt => truth(a < b),
        ScalarOp::Le => truth(a <= b),
        ScalarOp::Gt => truth(a > b),
        ScalarOp::Ge => truth(a >= b),
    }
}

/// `op` on two word integers, where the answer fits a word.
#[inline(always)]
pub fn ints(op: ScalarOp, a: i64, b: i64) -> Option<Value> {
    int_answer(
        op,
        a,
        b,
        |number| Some(Value::Int(number)),
        |truth| Some(Value::Bool(truth)),
    )
}

/// Arithmetic `op` on two word integers, where the answer fits a word;
/// `None` for a comparison and for division, whose answer is a float.
#[inline(always)]
pub fn int_number(op: ScalarOp, a: i64, b: i64) -> Option<i64> {
    int_answer(op, a, b, Some, |_| None)
}

/// Comparison `op` on two word integers; `None` for arithmetic.
#[inline(always)]
pub fn int_truth(op: ScalarOp, a: i64, b: i64) -> Option<bool> {
    int_answer(op, a, b, |_| None, Some)
}

/// `op`'s answer on two word integers, as `number` or `truth` takes it:
/// the one match every word integer path answers by.
#[inline(always)]
fn int_answer<T>(
    op: ScalarOp,
    a: i64,
    b: i64,
    number: impl FnOnce(i64) -> Option<T>,
    truth: impl FnOnce(bool) -> Option<T>,
) -> Option<T> {
    match op {
        ScalarOp::Add => number(a.checked_add(b)?),
        ScalarOp::Sub => number(a.checked_sub(b)?),
        ScalarOp::Mul => number(a.checked_mul(b)?),
        ScalarOp::RemTrunc => number(a.checked_rem(b)?),
        ScalarOp::RemFloor => {
            let r = a.checked_rem(b)?;
            number(if r != 0 && (r < 0) != (b < 0) {
                r.checked_add(b)?
            } else {
                r
            })
        }
        ScalarOp::FloorDiv => {
            let q = a.checked_div(b)?;
            let r = a.checked_rem(b)?;
            number(if r != 0 && (r < 0) != (b < 0) {
                q.checked_sub(1)?
            } else {
                q
            })
        }
        ScalarOp::Div => None,
        ScalarOp::Eq => truth(a == b),
        ScalarOp::Lt => truth(a < b),
        ScalarOp::Le => truth(a <= b),
        ScalarOp::Gt => truth(a > b),
        ScalarOp::Ge => truth(a >= b),
    }
}

/// Truncate a binary64 value and reduce it modulo 2^32. Non-finite values
/// and either zero give zero. No wide integer or floating remainder is needed.
#[inline(always)]
pub fn uint32(value: f64) -> u32 {
    if (i32::MIN as f64..=i32::MAX as f64).contains(&value) {
        return value as i32 as u32;
    }
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if !(0..84).contains(&exponent) {
        return 0;
    }
    let significand = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    let word = if exponent < 52 {
        (significand >> (52 - exponent)) as u32
    } else {
        (significand as u32) << (exponent - 52)
    };
    if bits >> 63 == 0 {
        word
    } else {
        word.wrapping_neg()
    }
}

/// A binary operation on truncated 32-bit Number operands. Keeping these
/// separate leaves the ordinary arithmetic kernel's dispatch unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordOp {
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Ushr,
}

/// Apply a 32-bit word operation, returning a binary64 Number. Signed
/// operations return an i32; unsigned right shift returns a u32.
#[inline(never)]
pub fn words(op: WordOp, a: Float, b: Float) -> Value {
    let (a, b) = (uint32(a.get()), uint32(b.get()));
    let value = match op {
        WordOp::And => (a & b) as i32 as f64,
        WordOp::Or => (a | b) as i32 as f64,
        WordOp::Xor => (a ^ b) as i32 as f64,
        WordOp::Shl => a.wrapping_shl(b & 31) as i32 as f64,
        WordOp::Shr => ((a as i32) >> (b & 31)) as f64,
        WordOp::Ushr => (a >> (b & 31)) as f64,
    };
    Value::Float(Float::new(value))
}

/// A text relation in UTF-16 code-unit order, without allocating code units.
#[inline]
pub fn texts(op: ScalarOp, a: &str, b: &str) -> Option<Value> {
    if !op.compares() {
        return None;
    }
    let order = if a.is_ascii() && b.is_ascii() {
        a.cmp(b)
    } else {
        a.encode_utf16().cmp(b.encode_utf16())
    };
    Some(Value::Bool(match op {
        ScalarOp::Eq => order.is_eq(),
        ScalarOp::Lt => order.is_lt(),
        ScalarOp::Le => !order.is_gt(),
        ScalarOp::Gt => order.is_gt(),
        ScalarOp::Ge => !order.is_lt(),
        _ => return None,
    }))
}
