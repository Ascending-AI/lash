//! Declaration-only handoff to lane C. No production region can be acquired.
#![allow(dead_code, reason = "lane C takes over after the C3 handoff")]
use super::KernelMachine;
use crate::compile::{region_isa::RegionCode, room::Refused};
use lash_kernel_doc::heap::HeapFault;
use std::{convert::Infallible, marker::PhantomData, sync::Arc};

pub(crate) struct CallActivation {
    sealed: Infallible,
}
pub(crate) struct RegionLease<'m> {
    sealed: Infallible,
    machine: PhantomData<&'m mut KernelMachine>,
}
pub(crate) struct ReachedPrefix {
    sealed: Infallible,
}
pub(crate) struct Reconstruction {
    sealed: Infallible,
}
pub(crate) struct MaterializedPrefix<'m> {
    sealed: Infallible,
    machine: PhantomData<&'m mut KernelMachine>,
}
pub(crate) struct PreparedRegionExit<'m> {
    sealed: Infallible,
    machine: PhantomData<&'m mut KernelMachine>,
}
pub(crate) enum StopAt {
    NextLegalPrefix,
    NextBoundary,
}
pub(crate) enum RegionStop {
    Boundary,
    Refusal,
    Throw,
    Slice,
    Observer,
    Handoff,
    Complete,
}
pub(crate) enum RegionError {
    Invalid(Refused),
    Heap(HeapFault),
}

impl<'m> RegionLease<'m> {
    pub(crate) fn acquire(
        _machine: &'m mut KernelMachine,
        _code: Arc<RegionCode>,
    ) -> Result<Self, RegionError> {
        Err(RegionError::Invalid(Refused::Source))
    }
    pub(crate) fn run(&mut self, _stop: StopAt) -> Result<RegionStop, RegionError> {
        Err(RegionError::Invalid(Refused::Source))
    }
    pub(crate) fn prefix(&self) -> &ReachedPrefix {
        match self.sealed {}
    }
    pub(crate) fn materialize(self) -> Result<MaterializedPrefix<'m>, RegionError> {
        Err(RegionError::Invalid(Refused::Source))
    }
}
impl<'m> MaterializedPrefix<'m> {
    pub(crate) fn reconstruction(&self) -> &Reconstruction {
        match self.sealed {}
    }
    pub(crate) fn prepare_exit(self) -> Result<PreparedRegionExit<'m>, RegionError> {
        Err(RegionError::Invalid(Refused::Source))
    }
}
impl PreparedRegionExit<'_> {
    pub(crate) fn commit(self) -> Result<RegionStop, RegionError> {
        Err(RegionError::Invalid(Refused::Source))
    }
}
