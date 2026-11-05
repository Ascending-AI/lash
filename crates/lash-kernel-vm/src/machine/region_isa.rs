//! Indexed private execution and affine publication of the reached prefix.
#![allow(
    dead_code,
    reason = "region execution is integrated after verified tables arrive"
)]
use super::KernelMachine;
use crate::compile::CodeId;
use crate::compile::region_isa::{CallId, RegionCode};
use crate::compile::room::region::calls::region_isa::RegionCalls;
use crate::compile::room::{Point, Refused};
use lash_kernel_doc::{Value, heap::HeapFault};
use std::{rc::Rc, sync::Arc};

mod code;
mod execute;
mod programs;
mod reconstruct;
mod registers;
mod returns;
mod source_counters;
use code::{Code, Limits};
use execute::Execution;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod laws;

pub(crate) struct CallActivation {
    lease: Rc<()>,
    invocation: u64,
    depth: usize,
    plan: u32,
    caller_program: CodeId,
}
pub(crate) struct RegionLease<'m> {
    authority: Authority<'m>,
    execution: Execution,
}
enum Authority<'m> {
    Machine {
        machine: &'m mut KernelMachine,
        code: Arc<RegionCode>,
    },
    #[cfg(test)]
    Fixture(fixtures::StateLease<'m>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReachedPrefix {
    program: CodeId,
    invocation: u64,
    ordinal: u64,
    occurrence: Option<u64>,
    prefix: u32,
    debt: u64,
}
pub(crate) struct Reconstruction {
    prefix: ReachedPrefix,
    frames: Box<[reconstruct::FrameImage]>,
    pending: Option<Value>,
    attempted: Option<Point>,
    counters: execute::Counters,
    stop: RegionStop,
    payload: Option<Value>,
    returning: Option<returns::ReturnImage>,
    source_counts: Box<[u64]>,
    entering: Option<u32>,
}
pub(crate) struct MaterializedPrefix<'m> {
    authority: Authority<'m>,
    reconstruction: Reconstruction,
}
pub(crate) struct PreparedRegionExit<'m> {
    prepared: PreparedAuthority<'m>,
    reconstruction: Reconstruction,
}
enum PreparedAuthority<'m> {
    #[cfg(test)]
    Fixture(fixtures::PreparedStateExit<'m>),
    Unavailable(std::marker::PhantomData<&'m mut KernelMachine>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopAt {
    NextLegalPrefix,
    NextBoundary,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionStop {
    Boundary,
    Refusal,
    Throw,
    Slice,
    Observer,
    Handoff,
    Complete,
}
#[derive(Debug)]
pub(crate) enum RegionError {
    Invalid(Refused),
    Heap(HeapFault),
}
fn invalid(reason: Refused) -> RegionError {
    RegionError::Invalid(reason)
}

impl<'m> RegionLease<'m> {
    pub(crate) fn acquire(
        machine: &'m mut KernelMachine,
        code: Arc<RegionCode>,
    ) -> Result<Self, RegionError> {
        let tables = Code::bind(machine, &code)?;
        let limits = Limits::machine(machine)?;
        let execution = Execution::acquire(tables, limits)?;
        Ok(Self {
            authority: Authority::Machine { machine, code },
            execution,
        })
    }
    pub(crate) fn run(&mut self, stop: StopAt) -> Result<RegionStop, RegionError> {
        self.execution.run(stop)
    }
    pub(crate) fn prefix(&self) -> &ReachedPrefix {
        &self.execution.reached
    }
    pub(crate) fn materialize(self) -> Result<MaterializedPrefix<'m>, RegionError> {
        let reconstruction = self.execution.reconstruct()?;
        Ok(MaterializedPrefix {
            authority: self.authority,
            reconstruction,
        })
    }
}
impl<'m> MaterializedPrefix<'m> {
    pub(crate) fn reconstruction(&self) -> &Reconstruction {
        &self.reconstruction
    }
    pub(crate) fn prepare_exit(self) -> Result<PreparedRegionExit<'m>, RegionError> {
        let prepared = self.authority.prepare(&self.reconstruction)?;
        Ok(PreparedRegionExit {
            prepared,
            reconstruction: self.reconstruction,
        })
    }
}
impl PreparedRegionExit<'_> {
    pub(crate) fn commit(self) -> Result<RegionStop, RegionError> {
        let stop = self.reconstruction.stop;
        self.prepared.commit(self.reconstruction)?;
        Ok(stop)
    }
}
impl<'m> Authority<'m> {
    fn prepare(
        self,
        reconstruction: &Reconstruction,
    ) -> Result<PreparedAuthority<'m>, RegionError> {
        let _ = &reconstruction;
        match self {
            Self::Machine { .. } => Err(invalid(Refused::Facts)),
            #[cfg(test)]
            Self::Fixture(authority) => Ok(PreparedAuthority::Fixture(
                authority.prepare(reconstruction)?,
            )),
        }
    }
}
impl PreparedAuthority<'_> {
    fn commit(self, reconstruction: Reconstruction) -> Result<(), RegionError> {
        let _ = &reconstruction;
        match self {
            #[cfg(test)]
            Self::Fixture(prepared) => prepared.commit(reconstruction),
            Self::Unavailable(_) => Err(invalid(Refused::Facts)),
        }
    }
}
impl RegionCalls for RegionLease<'_> {
    fn enter_call(&mut self, site: CallId) -> Result<CallActivation, RegionError> {
        let result = self.execution.enter_call(site.index());
        if result.is_err() && self.execution.stop != Some(RegionStop::Refusal) {
            self.execution.faulted = true;
        }
        result
    }
    fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        let result = self.execution.leave_call(activation);
        if result.is_err() && self.execution.stop != Some(RegionStop::Refusal) {
            self.execution.faulted = true;
        }
        result
    }
}
