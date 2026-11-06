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

mod actions;
mod code;
mod entry;
mod execute;
#[cfg(test)]
mod fixture_phases;
mod heap;
mod invocations;
mod live;
mod operands;
mod programs;
mod publication;
mod raise;
pub(in crate::machine) mod reconstruct;
mod registers;
mod returns;
mod runtime;
mod source_counters;
mod storage;
#[cfg(test)]
mod storage_fixture;
mod verified;
use code::{Code, Limits};
use execute::Execution;
pub(in crate::machine) use verified::restore_source_controls;
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
    execution: Executing<'m>,
}
enum Authority<'m> {
    Machine(publication::Authority<'m>),
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
    issued_invocations: u64,
    prefix: ReachedPrefix,
    frames: Box<[reconstruct::FrameImage]>,
    pending: Option<Value>,
    attempted: Option<Point>,
    counters: execute::Counters,
    /// The limits the run ended under, grown where a limiter grew them.
    limits: Limits,
    stop: RegionStop,
    payload: Option<Value>,
    returning: Option<returns::ReturnImage<Value>>,
    source_counts: Box<[u64]>,
    entering: Option<u32>,
    canonical: Option<publication::Canonical>,
    /// Main session cells this lease wrote, for the VM session table.
    sessions: Box<[(crate::compile::BindingName, Value)]>,
}
pub(crate) struct MaterializedPrefix<'m> {
    graph: FrozenGraph<'m>,
    authority: Authority<'m>,
    reconstruction: Reconstruction,
}
pub(crate) struct PreparedRegionExit<'m> {
    prepared: PreparedAuthority<'m>,
    reconstruction: Reconstruction,
}
#[cfg_attr(
    test,
    allow(
        clippy::large_enum_variant,
        reason = "the fixture variant is test-only; production keeps its affine transaction inline"
    )
)]
enum PreparedAuthority<'m> {
    #[cfg(test)]
    Fixture(fixtures::PreparedStateExit<'m>),
    Machine(publication::Prepared<'m>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopAt {
    NextLegalPrefix,
    NextBoundary,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionStop {
    Boundary,
    /// The region stands at a delivering action for the machine to perform.
    Action,
    /// The region stands before an effect, its value held by the frame's
    /// roles: the machine copies it out and the region takes the step.
    Effect(crate::compile::region_isa::EffectKind),
    /// The region accepted a finish or fail: the run ends with it.
    End,
    /// The region stands before a native call its checked kernel cannot
    /// answer in place; the machine answers it and the region takes it.
    Native,
    Refusal,
    Throw,
    /// The region accepted an uncaught ReturnVia, its operand still held:
    /// the machine returns it out of the frame through the policy.
    ReturnVia,
    Slice,
    Observer,
    Handoff,
    Complete,
}
/// A step the machine served outside the region for `task`'s top frame,
/// standing before the step at `at`: a native call's answer, or an effect
/// with none. The region takes that step at once.
pub(in crate::machine) struct Served {
    task: lash_kernel_doc::TaskId,
    at: Point,
    answer: Option<lash_kernel_state::kernel_internals::logical::NativeBatch>,
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
        Self::acquire_verified(machine, tables, limits)
    }

    pub(crate) fn run(&mut self, stop: StopAt) -> Result<RegionStop, RegionError> {
        self.execution.run(stop)
    }
    pub(crate) fn prefix(&self) -> &ReachedPrefix {
        self.execution.prefix()
    }
    pub(crate) fn materialize(self) -> Result<MaterializedPrefix<'m>, RegionError> {
        let (reconstruction, graph) = self.execution.reconstruct()?;
        Ok(MaterializedPrefix {
            authority: self.authority,
            reconstruction,
            graph,
        })
    }
}
impl<'m> MaterializedPrefix<'m> {
    pub(crate) fn reconstruction(&self) -> &Reconstruction {
        &self.reconstruction
    }
    /// The charge and memory limits the run ended under, grown where a
    /// limiter grew them.
    pub(crate) fn limits(&self) -> (u64, u64) {
        (
            self.reconstruction.limits.charge,
            self.reconstruction.limits.memory,
        )
    }
    pub(crate) fn prepare_exit(self) -> Result<PreparedRegionExit<'m>, RegionError> {
        let mut reconstruction = self.reconstruction;
        let prepared = self.authority.prepare(&mut reconstruction, self.graph)?;
        Ok(PreparedRegionExit {
            prepared,
            reconstruction,
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
        reconstruction: &mut Reconstruction,
        graph: FrozenGraph<'m>,
    ) -> Result<PreparedAuthority<'m>, RegionError> {
        let _ = (&reconstruction, &graph);
        match self {
            Self::Machine(authority) => authority
                .prepare(reconstruction, graph)
                .map(PreparedAuthority::Machine),
            #[cfg(test)]
            Self::Fixture(authority) => {
                let FrozenGraph::Fixture(graph) = graph else {
                    return Err(invalid(Refused::Facts));
                };
                Ok(PreparedAuthority::Fixture(
                    authority.prepare(reconstruction, graph)?,
                ))
            }
        }
    }
}
impl PreparedAuthority<'_> {
    fn commit(self, reconstruction: Reconstruction) -> Result<(), RegionError> {
        let _ = &reconstruction;
        match self {
            #[cfg(test)]
            Self::Fixture(prepared) => prepared.commit(reconstruction),
            Self::Machine(prepared) => prepared.commit(reconstruction),
        }
    }
}
impl RegionCalls for RegionLease<'_> {
    fn enter_call(&mut self, site: CallId) -> Result<CallActivation, RegionError> {
        let result = self.execution.enter_call(site.index());
        if result.is_err() && self.execution.stop() != Some(RegionStop::Refusal) {
            self.execution.fault();
        }
        result
    }
    fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        let result = self.execution.leave_call(activation);
        if result.is_err() && self.execution.stop() != Some(RegionStop::Refusal) {
            self.execution.fault();
        }
        result
    }
}

fn storage_fault(fault: lash_kernel_state::kernel_internals::logical::LeaseFault) -> RegionError {
    match fault {
        lash_kernel_state::kernel_internals::logical::LeaseFault::Heap(fault) => {
            RegionError::Heap(fault)
        }
        error => RegionError::Heap(HeapFault::Malformed(format!(
            "verified state operation failed: {error}"
        ))),
    }
}

#[cfg_attr(
    test,
    allow(
        clippy::large_enum_variant,
        reason = "the fixture variant is test-only; production execution stays inline"
    )
)]
enum Executing<'m> {
    Machine(verified::Running<'m>),
    #[cfg(test)]
    Fixture(Execution<storage_fixture::FixtureView>),
}
#[cfg_attr(
    test,
    allow(
        clippy::large_enum_variant,
        reason = "the fixture variant is test-only; production graph authority stays inline"
    )
)]
enum FrozenGraph<'m> {
    Machine {
        lease: lash_kernel_state::kernel_internals::logical::StateLease<'m>,
        graph: lash_kernel_state::kernel_internals::logical::LeaseGraphExit,
        invocations: Option<lash_kernel_state::kernel_internals::logical::InvocationRange>,
        publications: live::Published,
    },
    #[cfg(test)]
    Fixture(storage_fixture::FixtureGraph),
}
impl<'m> Executing<'m> {
    fn run(&mut self, stop: StopAt) -> Result<RegionStop, RegionError> {
        match self {
            Self::Machine(execution) => execution.run(stop),
            #[cfg(test)]
            Self::Fixture(execution) => execution.run(stop),
        }
    }
    fn prefix(&self) -> &ReachedPrefix {
        match self {
            Self::Machine(execution) => &execution.reached,
            #[cfg(test)]
            Self::Fixture(execution) => &execution.reached,
        }
    }
    fn stop(&self) -> Option<RegionStop> {
        match self {
            Self::Machine(execution) => execution.stop,
            #[cfg(test)]
            Self::Fixture(execution) => execution.stop,
        }
    }
    fn fault(&mut self) {
        match self {
            Self::Machine(execution) => execution.faulted = true,
            #[cfg(test)]
            Self::Fixture(execution) => execution.faulted = true,
        }
    }
    fn enter_call(&mut self, site: u32) -> Result<CallActivation, RegionError> {
        match self {
            Self::Machine(execution) => execution.enter_call(site),
            #[cfg(test)]
            Self::Fixture(execution) => execution.enter_call(site),
        }
    }
    fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError> {
        match self {
            Self::Machine(execution) => execution.leave_call(activation),
            #[cfg(test)]
            Self::Fixture(execution) => execution.leave_call(activation),
        }
    }
    fn reconstruct(self) -> Result<(Reconstruction, FrozenGraph<'m>), RegionError> {
        match self {
            Self::Machine(execution) => {
                let reconstruct::Finished {
                    image,
                    live,
                    view: lease,
                    graph,
                    invocations,
                } = execution.reconstruct()?;
                let publications = live.publish(&graph)?;
                Ok((
                    image,
                    FrozenGraph::Machine {
                        lease,
                        graph,
                        invocations,
                        publications,
                    },
                ))
            }
            #[cfg(test)]
            Self::Fixture(execution) => {
                let reconstruct::Finished {
                    image,
                    live: _live,
                    view: _view,
                    graph,
                    invocations,
                } = execution.reconstruct()?;
                if invocations.is_some() {
                    return Err(invalid(Refused::Facts));
                }
                Ok((image, FrozenGraph::Fixture(graph)))
            }
        }
    }
    #[cfg(test)]
    fn fixture(&self) -> &Execution<storage_fixture::FixtureView> {
        match self {
            Self::Fixture(execution) => execution,
            _ => panic!("fixture observation used on production authority"),
        }
    }
}

#[cfg(test)]
impl Executing<'_> {
    fn fixture_mut(&mut self) -> &mut Execution<storage_fixture::FixtureView> {
        match self {
            Self::Fixture(execution) => execution,
            _ => panic!("fixture mutation used on production authority"),
        }
    }
}
