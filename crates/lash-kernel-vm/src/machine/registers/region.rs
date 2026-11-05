//! A process-local physical position, discarded at canonical safe points.
use super::{HeapFault, Registers};
use crate::compile::region_isa::{Pc, PrefixId, RegionCode};
use std::sync::Arc;

pub(in crate::machine) struct RegionCursor {
    program: Arc<RegionCode>,
    invocation: u64,
    pc: Pc,
    prefix: PrefixId,
}
impl RegionCursor {
    pub(in crate::machine) fn new(
        program: Arc<RegionCode>,
        invocation: u64,
        pc: Pc,
        prefix: PrefixId,
    ) -> Self {
        Self {
            program,
            invocation,
            pc,
            prefix,
        }
    }
}
impl Registers {
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
