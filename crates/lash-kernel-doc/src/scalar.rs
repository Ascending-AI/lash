//! Typed scalar kernels: the answers of the numeric operators on two
//! floats or two word integers, shared by the library's prepared entries,
//! whose own paths they are, and the machine's typed operator paths.
//! A kernel answers only where its answer is the
//! entry's; `None` leaves the call to the entry's general path, which
//! answers or declines.

use crate::{Float, Value};

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

/// `op` on two floats.
#[inline(always)]
pub fn floats(op: ScalarOp, a: Float, b: Float) -> Option<Value> {
    let (a, b) = (a.get(), b.get());
    Some(match op {
        ScalarOp::Add => Value::Float(Float::new(a + b)),
        ScalarOp::Sub => Value::Float(Float::new(a - b)),
        ScalarOp::Mul => Value::Float(Float::new(a * b)),
        ScalarOp::Div => Value::Float(Float::new(a / b)),
        // `fmod` of integral operands below 2^63 in magnitude with a
        // nonzero divisor is their integer remainder, a zero keeping the
        // dividend's sign; every other pair is the general path's.
        ScalarOp::RemTrunc => {
            let (x, y) = (a as i64, b as i64);
            if !(x as f64 == a && y as f64 == b && x != i64::MAX && y != i64::MAX && y != 0) {
                return None;
            }
            Value::Float(Float::new(match x.wrapping_rem(y) {
                0 => 0.0_f64.copysign(a),
                r => r as f64,
            }))
        }
        ScalarOp::RemFloor | ScalarOp::FloorDiv => return None,
        ScalarOp::Eq => Value::Bool(a == b),
        ScalarOp::Lt => Value::Bool(a < b),
        ScalarOp::Le => Value::Bool(a <= b),
        ScalarOp::Gt => Value::Bool(a > b),
        ScalarOp::Ge => Value::Bool(a >= b),
    })
}

/// `op` on two word integers, where the answer fits a word.
#[inline(always)]
pub fn ints(op: ScalarOp, a: i64, b: i64) -> Option<Value> {
    Some(match op {
        ScalarOp::Add => Value::Int(a.checked_add(b)?),
        ScalarOp::Sub => Value::Int(a.checked_sub(b)?),
        ScalarOp::Mul => Value::Int(a.checked_mul(b)?),
        ScalarOp::RemTrunc => Value::Int(a.checked_rem(b)?),
        ScalarOp::RemFloor => {
            let r = a.checked_rem(b)?;
            Value::Int(if r != 0 && (r < 0) != (b < 0) {
                r.checked_add(b)?
            } else {
                r
            })
        }
        ScalarOp::FloorDiv => {
            let q = a.checked_div(b)?;
            let r = a.checked_rem(b)?;
            Value::Int(if r != 0 && (r < 0) != (b < 0) {
                q.checked_sub(1)?
            } else {
                q
            })
        }
        ScalarOp::Div => return None,
        ScalarOp::Eq => Value::Bool(a == b),
        ScalarOp::Lt => Value::Bool(a < b),
        ScalarOp::Le => Value::Bool(a <= b),
        ScalarOp::Gt => Value::Bool(a > b),
        ScalarOp::Ge => Value::Bool(a >= b),
    })
}
