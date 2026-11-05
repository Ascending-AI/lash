//! The canonical VM fields remain borrowed and unchanged during private work.
use super::*;
use crate::machine::{Frame, frames::Frames, registers::Registers, tasks::region::CounterLease};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::Owner;

pub(super) struct Authority<'m> {
    pub(super) frames: &'m mut Frames,
    pub(super) counters: CounterLease<'m>,
    pub(super) registers: &'m mut Registers,
    pub(super) charged: &'m mut u64,
    pub(super) logical_memory: &'m std::cell::Cell<u64>,
    pub(super) task: TaskId,
    pub(super) exe: Arc<crate::compile::Executable>,
    pub(super) entry: Box<Frame>,
    pub(super) inputs: Box<[Owner]>,
    pub(super) limits: Limits,
}
