//! Guest ToPrimitive (FIG-3652): the answers an instruction's guest
//! `valueOf`/`toString` calls gave, replayed when the instruction reruns.
//!
//! A heap coercion cannot call guest code: it runs inside one instruction,
//! and a guest call is a VM frame. So when ECMA-262's OrdinaryToPrimitive
//! reaches an object with its own callable `valueOf` or `toString`, the
//! coercion records what it needs and fails with
//! [`RuntimeError::GuestCoercionPending`]. The VM restores the instruction's
//! operands, runs the hook through its ordinary call path, and reruns the
//! instruction; this replay hands each coercion, in order, the primitive its
//! hook answered. The rerun is the same pure computation over the same
//! operands, so its coercions arrive in the same order.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt;

use super::*;

/// The order OrdinaryToPrimitive tries an object's methods in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrimitiveHint {
    /// `+`, `==` and every other operator: `valueOf` first.
    Default,
    /// ToNumber: `valueOf` first.
    Number,
    /// ToString and ToPropertyKey: `toString` first.
    String,
}

impl PrimitiveHint {
    /// The methods OrdinaryToPrimitive tries, in order.
    pub(crate) fn method_order(self) -> [&'static str; 2] {
        match self {
            Self::String => ["toString", "valueOf"],
            Self::Default | Self::Number => ["valueOf", "toString"],
        }
    }
}

/// What one guest ToPrimitive answered.
#[derive(Clone, Debug)]
pub(crate) enum GuestPrimitive {
    /// A hook returned this primitive.
    Value(Value),
    /// No hook answered and the built-in `Object.prototype.toString` did:
    /// the object's type tag, which the coercion answers as it would for an
    /// object with no hooks at all.
    Tag,
}

/// The coercion a rerun needs a hook to answer.
#[derive(Clone, Debug)]
pub(crate) struct GuestCoercionRequest {
    pub(crate) object: Value,
    pub(crate) hint: PrimitiveHint,
}

/// The replay a heap coercion reads. The heap is shared by reference with the
/// VM's async steps, so the two pieces a coercion writes through `&Heap` are
/// thread-safe; neither is ever contended.
#[derive(Debug, Default)]
pub(crate) struct GuestCoercionReplay {
    answers: Vec<GuestPrimitive>,
    cursor: AtomicUsize,
    pending: Mutex<Option<GuestCoercionRequest>>,
}

impl GuestCoercionReplay {
    /// Starts an instruction's run with the answers its earlier runs
    /// collected.
    pub(crate) fn load(&mut self, answers: Vec<GuestPrimitive>) {
        self.answers = answers;
        *self.cursor.get_mut() = 0;
        *self.pending.lock_recover() = None;
    }

    /// Ends the run: the answers go back to the VM, whatever it requested is
    /// taken with them.
    pub(crate) fn unload(&mut self) -> (Vec<GuestPrimitive>, Option<GuestCoercionRequest>) {
        *self.cursor.get_mut() = 0;
        (
            std::mem::take(&mut self.answers),
            self.pending.lock_recover().take(),
        )
    }

    fn next(&self, object: &Value, hint: PrimitiveHint) -> Result<GuestPrimitive, RuntimeError> {
        let cursor = self.cursor.load(Ordering::Relaxed);
        if let Some(answer) = self.answers.get(cursor) {
            self.cursor.store(cursor + 1, Ordering::Relaxed);
            return Ok(answer.clone());
        }
        *self.pending.lock_recover() = Some(GuestCoercionRequest {
            object: object.clone(),
            hint,
        });
        Err(RuntimeError::GuestCoercionPending)
    }
}

impl Heap {
    /// OrdinaryToPrimitive of an object with its own callable `toString` or
    /// `valueOf`: the answer this instruction's run replays, or the request
    /// that makes the VM call the hook.
    pub(super) fn guest_primitive(
        &self,
        object: &Value,
        hint: PrimitiveHint,
    ) -> Result<GuestPrimitive, RuntimeError> {
        self.guest_coercion.next(object, hint)
    }
}
