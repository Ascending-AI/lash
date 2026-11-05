//! Collection and JSON callbacks use the region's ordinary call instruction.
//! Source operands, captures and results stay in the same lease as the caller.
use super::*;
use crate::compile::region_isa::{CallId, RegionDraft};
use crate::compile::room::region::calls::region_isa::{CallBinding, CallPlan, CalleePlan};
use crate::compile::{CodeId, Executable, StmtId};
use lash_kernel_state::kernel_internals::logical::{LeaseFault, LeaseValue, StateLease};

// Retiring statement schedules still consume the source-route compiler. It
// supplies no callback execution, value evaluation or new region authority.
pub(in crate::compile::room) mod caller;

pub(crate) struct CallbackProgram {
    call: CallPlan,
}

/// Classification happens before invoking a callback. An opaque callable or
/// authority miss is a boundary at the preceding legal source prefix.
pub(crate) enum CallbackTarget<'plan, 'lease> {
    Declared(&'plan CalleePlan),
    Value(CallBinding<'plan, 'lease>),
}
impl CallbackTarget<'_, '_> {
    pub(crate) fn plan(&self) -> &CalleePlan {
        match self {
            Self::Declared(plan) => plan,
            Self::Value(binding) => binding.plan(),
        }
    }
}
impl CallbackProgram {
    /// Compile all admitted targets once. Actual arguments and captures are
    /// never supplied to this compiler and no candidate body is evaluated.
    pub(crate) fn compile(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
    ) -> Result<Self, Refused> {
        Ok(Self {
            call: CallPlan::compile(exe, caller, statement, pc)?,
        })
    }
    pub(crate) fn call(&self) -> &CallPlan {
        &self.call
    }
    pub(crate) fn verify_region(
        &self,
        exe: &Executable,
        draft: &RegionDraft,
        site: CallId,
    ) -> Result<(), Refused> {
        self.call.verify_region(exe, draft, site)
    }
    /// C executes the verified RegionOp::Call through RegionCalls. The same
    /// affine activation owns Enter/Leave, invocation, depth and ordered debt
    /// for comparator, key, reduction and JSON callback sites.
    pub(crate) fn classify<'plan, 'lease>(
        &'plan self,
        exe: &Executable,
        lease: &'lease mut StateLease<'_>,
        callable: Option<LeaseValue>,
    ) -> Result<Option<CallbackTarget<'plan, 'lease>>, LeaseFault> {
        if !self.call.belongs_to(exe) {
            return Ok(None);
        }
        if self.call.callable().is_some() {
            let Some(value) = callable else {
                return Ok(None);
            };
            self.call
                .bind_callable(exe, lease, value)
                .map(|binding| binding.map(CallbackTarget::Value))
        } else if callable.is_none() {
            Ok(self.call.static_target().map(CallbackTarget::Declared))
        } else {
            Ok(None)
        }
    }
}
