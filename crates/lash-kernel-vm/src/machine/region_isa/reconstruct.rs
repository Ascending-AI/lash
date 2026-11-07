use super::storage::View;
use super::*;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Debug)]
pub(in crate::machine) struct FrameImage<T = Value> {
    pub(in crate::machine) program: CodeId,
    pub(in crate::machine) invocation: u64,
    pub(in crate::machine) pc: u32,
    pub(in crate::machine) continuation: Point,
    pub(in crate::machine) roles: Box<[(Slot, T)]>,
    pub(in crate::machine) operands: Box<[T]>,
    pub(in crate::machine) pending: Option<T>,
    pub(in crate::machine) incoming: bool,
}

pub(super) struct Finished<V: View> {
    pub(super) image: Reconstruction,
    pub(super) live: super::live::Live<V::Value>,
    pub(super) view: V,
    pub(super) graph: V::Graph,
    pub(super) invocations: Option<lash_kernel_state::kernel_internals::logical::InvocationRange>,
}

impl KernelMachine {
    /// Rebind ephemeral ISA state from the restored canonical frames only.
    pub(in crate::machine) fn reconstruct_canonical_region(
        &mut self,
    ) -> Result<(), lash_kernel_doc::heap::HeapFault> {
        if self.current.is_some()
            || self.region.is_some()
            || self.regs.region_values().next().is_some()
        {
            return Err(HeapFault::Malformed(
                "canonical reopen has live execution authority".into(),
            ));
        }
        self.heap.validate_frame_continuations()?;
        self.tasks.reconstruct_region_context()?;
        self.publish_logical_meter()
    }
}
