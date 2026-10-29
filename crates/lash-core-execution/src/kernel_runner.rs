//! One kernel drive loop. Owners supply page lifetimes and acknowledged saves.

use lash_kernel_vm::{Bound, BoundExceeded, Bounds, End, Request, RunError, WaitId};

/// A quiet point, without the task table retained by the machine.
#[derive(Clone, Debug, PartialEq)]
pub struct MachinePark {
    pub requests: Vec<Request>,
    pub withdrawn: Vec<WaitId>,
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
    /// Apply withdrawn waits before a request-bound failure can end the run.
    fn withdrawn(&mut self, _waits: &[WaitId]) {}
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
                owner.withdrawn(&park.withdrawn);
                park.requests.retain(|request| {
                    let Request::Effect(effect) = request;
                    !park.withdrawn.contains(&effect.wait)
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
    let consumed = observed.map(|used| {
        if resource == R::Charges {
            used.min(exceeded.limit)
        } else {
            used
        }
    });
    lash_protocol_types::ExecutionBoundFact {
        resource,
        limit: exceeded.limit,
        observed,
        consumed,
        function: exceeded.function.clone(),
        site: exceeded.site.clone(),
    }
}
