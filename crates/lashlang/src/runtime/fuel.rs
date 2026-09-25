//! The work-unit accounting proportional intrinsics and operators charge to
//! the instruction counter: each unit is a byte of text read or written, a
//! collection member scanned or produced, or a heap object walked.

use crate::ast::BinaryOp;

use super::Value;

pub(crate) fn sorting_work(items: usize) -> usize {
    if items < 2 {
        return 0;
    }
    items.saturating_mul(usize::BITS.saturating_sub((items - 1).leading_zeros()) as usize)
}

/// The work units a proportional builtin or instruction reads or writes on
/// `value`: a byte of text or a member of a collection each count one — the
/// units `charge_intrinsic_work` documents. Scalars and handles carry none: a
/// scalar's work is constant, and work a projected or heap reference implies
/// is counted where its target is read. The count is shallow — a member's own
/// members are [`deep_proportional_units`]'s.
pub(crate) fn proportional_units(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Tuple(items) | Value::List(items) => items.len(),
        Value::Record(record) => record.len(),
        _ => 0,
    }
}

/// [`proportional_units`] counted transitively — every member at every depth,
/// plus the text of each key — for the intrinsics whose scan descends into
/// members, like `validate` or a deep equality.
pub(crate) fn deep_proportional_units(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Tuple(items) | Value::List(items) => {
            items.iter().fold(items.len(), |total, item| {
                total.saturating_add(deep_proportional_units(item))
            })
        }
        Value::Record(record) => record.iter().fold(record.len(), |total, (key, value)| {
            total
                .saturating_add(key.len())
                .saturating_add(deep_proportional_units(value))
        }),
        _ => 0,
    }
}

/// The proportional work `eval_binary_values` or `eval_compare_values`
/// performs for `op` on these operands: `+` copies its whole result, `in`
/// scans the haystack, `==`/`!=` reads both sides' members, and an ordering
/// reads until the texts differ — each unit a byte or an element as
/// [`proportional_units`] counts.
pub(crate) fn binary_op_work_units(left: &Value, op: BinaryOp, right: &Value) -> usize {
    match op {
        BinaryOp::Add => proportional_units(left).saturating_add(proportional_units(right)),
        BinaryOp::Equal | BinaryOp::NotEqual => {
            deep_proportional_units(left).saturating_add(deep_proportional_units(right))
        }
        BinaryOp::In => deep_proportional_units(right),
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            match (left, right) {
                (Value::String(left), Value::String(right)) => left.len().min(right.len()),
                _ => 0,
            }
        }
        _ => 0,
    }
}
