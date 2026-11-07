//! A process-local physical position, discarded at canonical safe points.
use super::{HeapFault, Registers};
use crate::compile::region_isa::{Pc, PrefixId, RegionCode};
use std::sync::Arc;

pub(in crate::machine) struct RegionCursor {
    program: Arc<RegionCode>,
    invocation: u64,
    pc: Pc,
    prefix: PrefixId,
    refusal: Option<Box<crate::BoundExceeded>>,
}
impl RegionCursor {
    pub(in crate::machine) fn new(
        program: Arc<RegionCode>,
        invocation: u64,
        pc: Pc,
        prefix: PrefixId,
        refusal: Option<Box<crate::BoundExceeded>>,
    ) -> Self {
        Self {
            program,
            invocation,
            pc,
            prefix,
            refusal,
        }
    }
}
impl Registers {
    /// A committed refusal is consumed by the driver before another operation.
    /// The payload was allocated before D's atomic register/frontier swap.
    pub(in crate::machine) fn take_region_refusal(&mut self) -> Option<Box<crate::BoundExceeded>> {
        self.region_cursor.as_mut()?.refusal.take()
    }

    /// The cursor grants no execution authority; acquisition still binds the
    /// source frontier, actual roles, invocation and D's exclusive entry stamp.
    pub(in crate::machine) fn region_position(
        &self,
        program: &RegionCode,
        invocation: u64,
    ) -> Result<Option<(Pc, PrefixId)>, HeapFault> {
        let Some(cursor) = &self.region_cursor else {
            return Ok(None);
        };
        if cursor.refusal.is_some() {
            return Err(HeapFault::Malformed(
                "a committed region refusal cannot resume execution".into(),
            ));
        }
        if cursor.invocation != invocation
            || cursor.program.executable() != program.executable()
            || cursor.program.code() != program.code()
            || !Arc::ptr_eq(cursor.program.source(), program.source())
            || program.step(cursor.pc).is_none()
            || program.recovery(cursor.prefix).is_none()
        {
            return Err(HeapFault::Malformed(
                "physical region cursor belongs to another source activation".into(),
            ));
        }
        Ok(Some((cursor.pc, cursor.prefix)))
    }
}

impl Registers {
    pub(in crate::machine) fn region_frames_image(
        task: lash_kernel_doc::TaskId,
        frames: &[crate::machine::region_isa::reconstruct::FrameImage],
        cursor: RegionCursor,
    ) -> Result<Self, HeapFault> {
        let mut result = Self {
            region_cursor: Some(cursor),
            ..Self::default()
        };
        for frame in frames {
            for (slot, value) in &frame.roles {
                if !matches!(
                    slot,
                    lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
                        | lash_kernel_state::kernel_internals::logical::Slot::Capture(_)
                ) {
                    result.region.push((
                        lash_kernel_state::kernel_internals::logical::Owner::Value {
                            task,
                            invocation: frame.invocation,
                            slot: *slot,
                        },
                        *value,
                    ));
                }
            }
        }
        Ok(result)
    }
}
