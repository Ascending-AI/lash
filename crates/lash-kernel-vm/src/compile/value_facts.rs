//! Canonical scalar and resident indexed reads.
use lash_kernel_doc::Value;

/// The values a region holds in registers and slots.
#[inline(always)]
pub(crate) fn scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}

/// Which of a call's operands are constructions of no members, by
/// position: a node's result whose measurements its kind proves
/// ([`crate::data::empty_measure`]), which the call's charge takes from it
/// rather than from the arena.
#[derive(Clone, Copy, Default)]
pub(crate) struct Fresh(u16);

impl Fresh {
    /// The operands whose positions `mask` sets.
    pub(crate) fn of(mask: u16) -> Self {
        Self(mask)
    }

    /// Whether the operand at `at` is a construction of no members.
    #[inline(always)]
    pub(crate) fn empty(self, at: usize) -> bool {
        at < 16 && self.0 & (1 << at) != 0
    }
}

/// The list position `index` is, as the reference reads it; `None` where
/// it raises.
pub(crate) fn position(index: &Value) -> Option<u64> {
    match index {
        Value::Int(word) => u64::try_from(*word).ok(),
        Value::Float(float) => {
            let float = float.get();
            (float.fract() == 0.0 && float >= 0.0 && float < usize::MAX as f64)
                .then_some(float as u64)
        }
        _ => None,
    }
}
