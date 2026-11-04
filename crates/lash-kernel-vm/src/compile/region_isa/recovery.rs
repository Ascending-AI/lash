//! Bounded cold reconstruction tables; no history of loop occurrences.
use super::Reg;
use crate::compile::room::Point;
use lash_kernel_doc::Value;
use lash_kernel_state::kernel_internals::logical::Slot;

pub(crate) struct RecoveryMap {
    pub(crate) roles: Box<[(Slot, Reg<Value>)]>,
    pub(crate) operands: Box<[Reg<Value>]>,
    pub(crate) pending: Option<Reg<Value>>,
    pub(crate) continuation: Point,
}
