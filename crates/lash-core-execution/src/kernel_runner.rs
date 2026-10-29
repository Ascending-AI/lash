//! One kernel drive loop. Owners supply page lifetimes and acknowledged saves.

use lash_kernel_vm::{Bound, BoundExceeded, Bounds, End, Request, RunError, WaitId};

/// A quiet point, without the task table retained by the machine.
#[derive(Clone, Debug, PartialEq)]
pub struct MachinePark {
    pub requests: Vec<Request>,
    pub cancel_requests: Vec<WaitId>,
    pub outstanding: u64,
    pub live: u64,
}

/// The result of one bounded slice of a kernel document.
#[derive(Clone, Debug, PartialEq)]
pub enum MachineStep {
    Parked(MachinePark),
    Slice,
    Ended(End),
}

/// The state that differs between a cell and a durable process activation.
///
/// `next` owns the opened machine and its page leases. A park either saves
/// and delivers committed outcomes, returning `None`, or returns a candidate
/// to the outer owner. A returned candidate is not an acknowledged save.
#[async_trait::async_trait]
pub trait KernelOwner: Send {
    type Error: Send;
    type Output: Send;

    fn bounds(&self) -> Bounds;
    /// Collect explicit call cancellation before a request-bound failure can end the run.
    fn cancel_requests(&mut self, _waits: &[WaitId]) {}
    async fn next(&mut self) -> Result<MachineStep, Self::Error>;
    async fn sliced(&mut self) -> Result<Option<Self::Output>, Self::Error>;
    async fn park(&mut self, park: MachinePark) -> Result<Option<Self::Output>, Self::Error>;
    async fn end(&mut self, end: End) -> Result<Self::Output, Self::Error>;
}

/// Drive ready work and hand every quiet point to its saving owner.
pub async fn run<O: KernelOwner>(owner: &mut O) -> Result<O::Output, O::Error> {
    loop {
        match owner.next().await? {
            MachineStep::Slice => {
                if let Some(output) = owner.sliced().await? {
                    return Ok(output);
                }
            }
            MachineStep::Ended(end) => return owner.end(end).await,
            MachineStep::Parked(mut park) => {
                owner.cancel_requests(&park.cancel_requests);
                park.requests.retain(|request| {
                    let Request::Effect(effect) = request;
                    !park.cancel_requests.contains(&effect.wait)
                });
                if let Some(exceeded) = park_bound(&owner.bounds(), park.requests.len()) {
                    return owner.end(End::Error(RunError::Bound(exceeded))).await;
                }
                if let Some(output) = owner.park(park).await? {
                    return Ok(output);
                }
            }
        }
    }
}

/// Project the one tool input record. An omitted operand is an empty object.
pub fn tool_input<T, E>(
    args: &[T],
    project: impl FnOnce(&T) -> Result<serde_json::Value, E>,
    excess: impl FnOnce(usize) -> E,
) -> Result<serde_json::Value, E> {
    match args {
        [] => Ok(serde_json::Value::Object(serde_json::Map::new())),
        [input] => project(input),
        more => Err(excess(more.len())),
    }
}

/// K-BND-001: a park exceeding its request bound admits none of its requests.
pub fn park_bound(bounds: &Bounds, requests: usize) -> Option<BoundExceeded> {
    (requests > bounds.requests_per_park as usize).then(|| BoundExceeded {
        bound: Bound::RequestsPerPark,
        limit: u64::from(bounds.requests_per_park),
        function: None,
        site: None,
    })
}

/// Shared saturation and unknown-reading rules for kernel-bound evidence.
pub fn bound_fact(
    exceeded: &BoundExceeded,
    meters: Option<lash_kernel_vm::Meters>,
) -> lash_protocol_types::ExecutionBoundFact {
    use lash_protocol_types::CellResource as R;
    let (resource, observed) = match &exceeded.bound {
        Bound::Charge => (R::Charges, meters.and_then(|meters| meters.charged)),
        Bound::Memory => (R::MemoryBytes, meters.map(|meters| meters.memory)),
        Bound::LiveTasks => (
            R::LiveTasks,
            meters.map(|meters| u64::from(meters.live_tasks)),
        ),
        Bound::CallDepth => (R::CallDepth, None),
        Bound::RequestsPerPark => (R::RequestsPerPark, None),
        Bound::JoinMembers => (R::JoinMembers, None),
        Bound::Effects => (R::Effects, None),
        Bound::CheckpointBytes => (R::CheckpointBytes, None),
        Bound::Guard { .. } => (R::LibraryWork, None),
    };
    make_bound_fact(
        resource,
        exceeded.limit,
        observed,
        exceeded.function.clone(),
        exceeded.site.clone(),
    )
}

/// Receipts available at an execution decision. Absence is never a zero reading.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoundReadings {
    pub charged: Option<u64>,
    pub memory: Option<u64>,
    pub worker_cpu_nanos: Option<u64>,
    pub held_wall_nanos: Option<u64>,
}

/// Project a supervisor limit through the same normalization as a kernel bound.
/// Host replacement-attempt limits are infrastructure verdicts, not run bounds.
pub fn worker_bound_fact(
    limit: lash_protocol_types::worker_limit::WorkerLimit,
    bounds: &Bounds,
    readings: BoundReadings,
) -> Option<lash_protocol_types::ExecutionBoundFact> {
    use lash_protocol_types::CellResource as R;
    use lash_protocol_types::worker_limit::WorkerLimit as W;
    let nanos = |ms: u64| ms.saturating_mul(1_000_000);
    let (resource, limit, observed) = match limit {
        W::Fuel => (R::Charges, bounds.charge_limit, readings.charged),
        W::Heap => (R::MemoryBytes, bounds.memory, readings.memory),
        W::Observations => (R::MemoryBytes, bounds.memory, None),
        W::Depth => (R::CallDepth, u64::from(bounds.call_depth), None),
        W::Cpu { bound_ms } => (
            R::WorkerCpuNanos,
            nanos(bound_ms),
            readings.worker_cpu_nanos,
        ),
        W::Memory { bound_bytes } => (R::WorkerMemoryBytes, bound_bytes, None),
        W::Wall { bound_ms } => (R::HeldWallNanos, nanos(bound_ms), readings.held_wall_nanos),
        W::Deadline { bound_ms } => (R::WorkerDeadlineNanos, nanos(bound_ms), None),
        W::HostRead { bound_ms } => (R::HostReadNanos, nanos(bound_ms), None),
        W::EffectValue { size, bound } => (R::EffectValueBytes, bound, Some(size)),
        W::VmState { size, bound } => (R::VmStateBytes, bound, Some(size)),
        W::Frame { size, bound, .. } => (R::FrameBytes, bound, Some(size)),
        W::Attempts { .. } => return None,
    };
    Some(make_bound_fact(resource, limit, observed, None, None))
}

fn make_bound_fact(
    resource: lash_protocol_types::CellResource,
    limit: u64,
    observed: Option<u64>,
    function: Option<lash_kernel_value::names::FunctionName>,
    site: Option<Box<lash_kernel_value::sites::Site>>,
) -> lash_protocol_types::ExecutionBoundFact {
    let consumed = observed.map(|used| {
        if resource == lash_protocol_types::CellResource::Charges {
            used.min(limit)
        } else {
            used
        }
    });
    lash_protocol_types::ExecutionBoundFact {
        resource,
        limit,
        observed,
        consumed,
        function,
        site,
    }
}
