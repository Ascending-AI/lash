//! Indexed private execution and affine publication of the reached prefix.
use super::KernelMachine;
use crate::compile::CodeId;
use crate::compile::region_isa::{CallId, RegionCode};
use crate::compile::room::region::calls::region_isa::RegionCalls;
use crate::compile::room::{Point, Refused};
use lash_kernel_doc::{Value, heap::HeapFault};
use std::{rc::Rc, sync::Arc};

mod actions;
mod code;
mod invocations;
mod live;
mod operands;
mod publication;
mod raise;
pub(in crate::machine) mod reconstruct;
mod registers;
mod runtime;
mod source_counters;
mod storage;
mod verified;
use code::Limits;
pub(in crate::machine) use verified::{BoundCodes, restore_source_controls};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Counters {
    pub(super) charge: u64,
    pub(super) progress: u64,
    pub(super) balance: u64,
    pub(super) peak: u64,
    pub(super) depth: u32,
    pub(super) debt: u64,
}
pub(crate) struct CallActivation {
    lease: Rc<()>,
    invocation: u64,
    depth: usize,
    plan: u32,
    caller_program: CodeId,
}
pub(crate) struct RegionLease<'m> {
    authority: publication::Authority<'m>,
    execution: verified::Running<'m>,
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
    frames: Box<[reconstruct::FrameImage]>,
    attempted: Option<Point>,
    counters: Counters,
    /// The limits the run ended under, grown where a limiter grew them.
    limits: Limits,
    stop: RegionStop,
    payload: Option<Value>,
    source_counts: Box<[u64]>,
    canonical: Option<publication::Canonical>,
    /// Main session cells this lease wrote, for the VM session table.
    sessions: Box<[(crate::compile::BindingName, Value)]>,
}
pub(crate) struct MaterializedPrefix<'m> {
    graph: FrozenGraph<'m>,
    authority: publication::Authority<'m>,
    reconstruction: Reconstruction,
}
pub(crate) struct PreparedRegionExit<'m> {
    prepared: publication::Prepared<'m>,
    reconstruction: Reconstruction,
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
    /// The lease's root frame stands before its Exit, its value in its
    /// departure role, for the machine to complete: it returns into a
    /// caller the machine holds, or its completion is the machine's.
    Leave,
    /// The region stands before an effect, its value held by the frame's
    /// roles: the machine copies it out and the region takes the step.
    Effect(crate::compile::region_isa::EffectKind),
    /// The region accepted a finish or fail: the run ends with it.
    End,
    /// The region stands before a native call its checked kernel cannot
    /// answer in place; the machine answers it and the region takes it.
    Native,
    /// The region stands before a dynamic call whose target its lease has
    /// not bound; the machine binds it and the region takes the call.
    Bind,
    /// The region stands at a step the source refuses with a typed error:
    /// the machine raises it there, as it raises a native's error.
    Refused {
        kind: &'static str,
        message: &'static str,
    },
    /// The region stands before a checked helper's call whose arguments it
    /// cannot show fit the helper's signature: the machine checks them,
    /// raising `function_argument`, or the region takes the call.
    Check,
    Refusal,
    Throw,
    Slice,
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
    pub(crate) fn run(&mut self, stop: StopAt) -> Result<RegionStop, RegionError> {
        self.execution.run(stop)
    }
    pub(crate) fn materialize(self) -> Result<MaterializedPrefix<'m>, RegionError> {
        let reconstruct::Finished {
            image,
            live,
            view: lease,
            graph,
            invocations,
        } = self.execution.reconstruct()?;
        let publications = live.publish(&graph)?;
        Ok(MaterializedPrefix {
            authority: self.authority,
            reconstruction: image,
            graph: FrozenGraph {
                lease,
                graph,
                invocations,
                publications,
            },
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

struct FrozenGraph<'m> {
    lease: lash_kernel_state::kernel_internals::logical::StateLease<'m>,
    graph: lash_kernel_state::kernel_internals::logical::LeaseGraphExit,
    invocations: Option<lash_kernel_state::kernel_internals::logical::InvocationRange>,
    publications: live::Published,
}
