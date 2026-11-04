//! Frozen call seam. Lane B takes ownership after the C4 handoff.
#![allow(dead_code, reason = "ISA call composition follows C4 handoff")]
use crate::compile::{
    Executable,
    region_isa::{CallId, RegionDraft},
    room::Refused,
};
use crate::machine::region_isa::{CallActivation, RegionError};

pub(crate) struct CallPlan {
    sealed: std::convert::Infallible,
}

pub(crate) fn compile_calls(
    _exe: &Executable,
    draft: &RegionDraft,
) -> Result<Box<[CallPlan]>, Refused> {
    if draft.has_calls() {
        Err(Refused::Source)
    } else {
        Ok(Box::new([]))
    }
}

pub(crate) trait RegionCalls {
    fn enter_call(&mut self, site: CallId) -> Result<CallActivation, RegionError>;
    fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError>;
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod fixtures {
    use super::CallPlan;
    pub(crate) fn empty_plan() -> Box<[CallPlan]> {
        Box::new([])
    }
}
