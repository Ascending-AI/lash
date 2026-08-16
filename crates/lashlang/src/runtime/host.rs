use crate::{HostRequirementsRef, LashlangExecutionCallSite, ModuleRef, ProcessRef};

use super::{ExecutionScratch, ProfileReport, ProjectedBindings, Record, RuntimeFailure, Value};
use crate::LashlangExecutionObservation;
use lash_sansio::sync::MutexExt;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, Debug)]
pub enum AbilityOp {
    ResourceOperation(ResourceOperation),
    ResourceOperationBatch(ResourceOperationBatch),
    Await(Value),
    Cancel(Value),
    Print(Value),
    Finish(Value),
    Fail(Value),
    StartProcess(Box<ProcessStart>),
    ProcessEvent(ProcessEvent),
    Sleep(Sleep),
    WaitSignal { name: String },
    SignalRun(ProcessSignal),
}

#[derive(Clone, Debug)]
pub enum AbilityResult {
    Value(Value),
    ResourceOperationBatch(ResourceOperationBatchResult),
    Unit,
}

impl AbilityResult {
    pub fn into_value(self, op: &'static str) -> Result<Value, ExecutionHostError> {
        match self {
            Self::Value(value) => Ok(value),
            Self::ResourceOperationBatch(_) => Err(ExecutionHostError::new(format!(
                "{op} returned a resource operation batch result"
            ))),
            Self::Unit => Err(ExecutionHostError::new(format!("{op} returned no value"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessStart {
    pub module_ref: ModuleRef,
    pub process_ref: ProcessRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub start_site: LashlangExecutionCallSite,
    pub process_name: String,
    pub args: Record,
}

#[derive(Clone, Debug)]
pub struct ResourceOperation {
    pub receiver: Value,
    pub operation: String,
    pub args: Vec<Value>,
    pub call_site: Option<crate::LashlangExecutionCallSite>,
}

#[derive(Clone, Debug)]
pub struct ResourceOperationBatch {
    pub operations: Vec<ResourceOperation>,
}

#[derive(Clone, Debug)]
pub struct ResourceOperationBatchResult {
    /// Per-leaf results in the batch's **input** order. Aggregates that are
    /// specified to preserve input order — `Promise.allSettled` — read this
    /// directly.
    pub results: Vec<ResourceOperationResult>,
    /// Input indices in the order the leaves **settled**, which is the order
    /// `Promise.all` must select its rejection from. Hosts record it as part of
    /// the journaled batch result so replay reproduces the same choice.
    ///
    /// It is a required field rather than an optional one: a host that cannot
    /// observe settlement order must say so explicitly with
    /// [`ResourceOperationBatchResult::settled_in_input_order`], because a
    /// silently defaulted order would reintroduce the input-order rejection
    /// this field exists to fix.
    pub settlement_order: Vec<usize>,
}

impl ResourceOperationBatchResult {
    /// A batch whose leaves settled in the order they were issued.
    ///
    /// This is the honest constructor for a host that runs its leaves
    /// sequentially, and for one that resolves them without ever suspending:
    /// in both cases input order *is* settlement order.
    pub fn settled_in_input_order(results: Vec<ResourceOperationResult>) -> Self {
        let settlement_order = (0..results.len()).collect();
        Self {
            results,
            settlement_order,
        }
    }

    /// A batch whose leaves settled in `settlement_order`, an ordering of the
    /// input indices.
    pub fn settled_in_order(
        results: Vec<ResourceOperationResult>,
        settlement_order: Vec<usize>,
    ) -> Self {
        Self {
            results,
            settlement_order,
        }
    }

    /// The input indices in settlement order, validated against `results`.
    ///
    /// The VM refuses a batch whose order is not an ordering of its results
    /// rather than guessing, so a host that miscounts fails closed. The error
    /// names the offending position: a length-only complaint reads as
    /// self-consistent and leaves nothing to debug.
    pub fn settlement_sequence(&self) -> Result<&[usize], String> {
        if self.settlement_order.len() != self.results.len() {
            return Err(format!(
                "reported {} settled positions for {} results",
                self.settlement_order.len(),
                self.results.len()
            ));
        }
        let mut seen = vec![false; self.results.len()];
        for index in &self.settlement_order {
            let Some(slot) = seen.get_mut(*index) else {
                return Err(format!(
                    "settled position {index} is out of range for {} results",
                    self.results.len()
                ));
            };
            if *slot {
                return Err(format!("settled position {index} was reported twice"));
            }
            *slot = true;
        }
        Ok(&self.settlement_order)
    }
}

#[derive(Clone, Debug)]
pub enum ResourceOperationResult {
    Value(Value),
    Error(ExecutionHostError),
}

impl ResourceOperationResult {
    pub fn from_result(result: Result<Value, ExecutionHostError>) -> Self {
        match result {
            Ok(value) => Self::Value(value),
            Err(error) => Self::Error(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEventKind {
    Yield,
    Wake,
}

#[derive(Clone, Debug)]
pub struct ProcessEvent {
    pub kind: ProcessEventKind,
    pub value: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepKind {
    For,
    Until,
}

#[derive(Clone, Debug)]
pub struct Sleep {
    pub kind: SleepKind,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub struct ProcessSignal {
    pub run: Value,
    pub name: String,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExecutionMode {
    #[default]
    Foreground,
    Process,
}

/// An explicit finite execution limit or an explicit opt-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionBound<T> {
    Bounded(T),
    Unbounded,
}

impl ExecutionBound<std::num::NonZeroU64> {
    /// Construct a finite instruction budget.
    ///
    /// # Panics
    ///
    /// Panics when `instructions` is zero.
    pub const fn instructions(instructions: u64) -> Self {
        match std::num::NonZeroU64::new(instructions) {
            Some(instructions) => Self::Bounded(instructions),
            None => panic!("instruction budget must be non-zero"),
        }
    }

    /// Construct a finite logical-memory bound in bytes.
    ///
    /// The same nonzero representation carries instruction counts and byte
    /// counts; naming both constructors keeps a byte limit from being spelled
    /// as an instruction budget at the call site.
    ///
    /// # Panics
    ///
    /// Panics when `bytes` is zero.
    pub const fn logical_bytes(bytes: u64) -> Self {
        match std::num::NonZeroU64::new(bytes) {
            Some(bytes) => Self::Bounded(bytes),
            None => panic!("logical memory limit must be non-zero"),
        }
    }
}

impl ExecutionBound<Duration> {
    /// Construct a finite active-VM deadline in milliseconds.
    pub const fn millis(milliseconds: u64) -> Self {
        Self::Bounded(Duration::from_millis(milliseconds))
    }

    /// Construct a finite active-VM deadline in seconds.
    pub const fn secs(seconds: u64) -> Self {
        Self::Bounded(Duration::from_secs(seconds))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionBoundWire<T> {
    Bounded(T),
    Unbounded,
}

impl serde::Serialize for ExecutionBound<std::num::NonZeroU64> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Bounded(value) => ExecutionBoundWire::Bounded(*value).serialize(serializer),
            Self::Unbounded => {
                ExecutionBoundWire::<std::num::NonZeroU64>::Unbounded.serialize(serializer)
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionBound<std::num::NonZeroU64> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match ExecutionBoundWire::deserialize(deserializer)? {
            ExecutionBoundWire::Bounded(value) => Self::Bounded(value),
            ExecutionBoundWire::Unbounded => Self::Unbounded,
        })
    }
}

impl serde::Serialize for ExecutionBound<Duration> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Bounded(value) => {
                let milliseconds =
                    u64::try_from(value.as_millis()).map_err(serde::ser::Error::custom)?;
                ExecutionBoundWire::Bounded(milliseconds).serialize(serializer)
            }
            Self::Unbounded => ExecutionBoundWire::<u64>::Unbounded.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionBound<Duration> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match ExecutionBoundWire::<u64>::deserialize(deserializer)? {
                ExecutionBoundWire::Bounded(milliseconds) => Self::millis(milliseconds),
                ExecutionBoundWire::Unbounded => Self::Unbounded,
            },
        )
    }
}

/// Independent limits for active Lashlang VM execution.
///
/// Foreground executions receive fresh meters for each block. Durable process
/// executions persist both meters in every continuation, so the limits are
/// cumulative across segment handovers for the process's entire life. The
/// deadline counts active VM time only: time parked on awaited host effects is
/// excluded. Enforcement occurs after intrinsic dispatch, before and after
/// effects, at cooperative yields, and at terminal VM exits, so instruction
/// and time limits can overshoot only by one bounded dispatch/check interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub instruction_budget: ExecutionBound<std::num::NonZeroU64>,
    pub deadline: ExecutionBound<Duration>,
    pub memory_limit: ExecutionBound<std::num::NonZeroU64>,
    pub max_frame_depth: std::num::NonZeroU64,
}

pub const DEFAULT_MAX_VM_FRAME_DEPTH: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(1_024).expect("the default frame depth is nonzero");

/// The logical-memory ceiling an execution gets when its host does not state
/// one.
///
/// Generous rather than tight: it is not a policy, it is the backstop that
/// keeps a host which never thought about memory from handing the guest the
/// whole machine. Before it existed the trait default was `Unbounded`, which
/// set the heap's limit to `u64::MAX` and made every pre-charge — including
/// the one array construction relies on — arithmetically incapable of
/// tripping, so `Array.from({ length: 1e9 })` walked the process into the
/// OOM killer instead of returning `MemoryLimitExceeded`.
///
/// 512 MiB is well above anything a real cell holds (the heap's own standalone
/// default is 64 MiB) and well below what a host process can absorb. A host
/// that genuinely wants no ceiling still overrides `execution_bounds` with
/// `ExecutionBound::Unbounded` — the point is that it must say so.
pub const DEFAULT_HOST_MEMORY_LIMIT_BYTES: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(512 * 1024 * 1024).expect("the default memory limit is nonzero");

impl ExecutionBounds {
    /// Builds a bound set.
    ///
    /// All three limits are stated: a host that does not decide how much
    /// logical memory an execution may hold has not finished configuring it,
    /// and a silent default here would be a bound nobody chose.
    pub const fn new(
        instruction_budget: ExecutionBound<std::num::NonZeroU64>,
        deadline: ExecutionBound<Duration>,
        memory_limit: ExecutionBound<std::num::NonZeroU64>,
    ) -> Self {
        Self {
            instruction_budget,
            deadline,
            memory_limit,
            max_frame_depth: DEFAULT_MAX_VM_FRAME_DEPTH,
        }
    }

    pub const fn with_max_frame_depth(mut self, max_frame_depth: std::num::NonZeroU64) -> Self {
        self.max_frame_depth = max_frame_depth;
        self
    }

    pub const fn with_memory_limit(
        mut self,
        memory_limit: ExecutionBound<std::num::NonZeroU64>,
    ) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// Unbounded time and instructions with the default memory ceiling: what a
    /// host gets when it does not override `execution_bounds`.
    pub const fn memory_bounded_default() -> Self {
        Self::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
            ExecutionBound::Bounded(DEFAULT_HOST_MEMORY_LIMIT_BYTES),
        )
    }

    pub const fn unbounded() -> Self {
        Self::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
        )
    }
}

pub trait ExecutionHost: Sync {
    fn perform(
        &self,
        op: AbilityOp,
    ) -> impl Future<Output = Result<AbilityResult, ExecutionHostError>> + Send;

    fn yield_now(&self) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Foreground
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        ProjectedBindings::default()
    }

    fn trace_runtime_errors(&self) -> bool {
        false
    }

    fn profile_execution(&self) -> bool {
        false
    }

    /// Time and instructions are the host's business; memory is not left
    /// open. See [`DEFAULT_HOST_MEMORY_LIMIT_BYTES`].
    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::memory_bounded_default()
    }

    /// Cheap cooperative cancellation probe. Like execution bounds, this is a
    /// host terminal and therefore bypasses guest exception handlers.
    fn is_cancelled(&self) -> bool {
        false
    }

    /// Deterministic GC stress mode used by the conformance suite.
    fn collect_heap_every_allocation(&self) -> bool {
        false
    }

    fn take_scratch(&self) -> Option<ExecutionScratch> {
        None
    }

    fn store_scratch(&self, _scratch: ExecutionScratch) {}

    fn observe_runtime_failure(&self, _failure: RuntimeFailure) {}

    fn observe_profile(&self, _profile: ProfileReport) {}

    fn observe_lashlang_execution(&self, _observation: LashlangExecutionObservation) {}
}

pub struct ExecutionEnvironment<'host, H: ExecutionHost> {
    host: &'host H,
    mode: ExecutionMode,
    projected: ProjectedBindings,
    scratch: Mutex<Option<ExecutionScratch>>,
    trace_runtime_errors: bool,
    profile_execution: bool,
    execution_bounds: ExecutionBounds,
    runtime_failure: Mutex<Option<RuntimeFailure>>,
    profile: Mutex<Option<ProfileReport>>,
}

impl<'host, H: ExecutionHost> ExecutionEnvironment<'host, H> {
    pub fn new(host: &'host H) -> Self {
        Self {
            host,
            mode: host.execution_mode(),
            projected: host.projected_bindings(),
            scratch: Mutex::new(host.take_scratch()),
            trace_runtime_errors: host.trace_runtime_errors(),
            profile_execution: host.profile_execution(),
            execution_bounds: host.execution_bounds(),
            runtime_failure: Mutex::new(None),
            profile: Mutex::new(None),
        }
    }

    pub fn with_mode(mut self, mode: ExecutionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn process(self) -> Self {
        self.with_mode(ExecutionMode::Process)
    }

    pub fn foreground(self) -> Self {
        self.with_mode(ExecutionMode::Foreground)
    }

    pub fn with_projected_bindings(mut self, projected: ProjectedBindings) -> Self {
        self.projected = projected;
        self
    }

    pub fn with_scratch(mut self, scratch: ExecutionScratch) -> Self {
        self.scratch = Mutex::new(Some(scratch));
        self
    }

    pub fn traced(mut self) -> Self {
        self.trace_runtime_errors = true;
        self
    }

    pub fn profiled(mut self) -> Self {
        self.profile_execution = true;
        self
    }

    pub fn with_execution_bounds(mut self, execution_bounds: ExecutionBounds) -> Self {
        self.execution_bounds = execution_bounds;
        self
    }

    pub fn take_runtime_failure(&self) -> Option<RuntimeFailure> {
        self.runtime_failure.lock_recover().take()
    }

    pub fn take_profile(&self) -> Option<ProfileReport> {
        self.profile.lock_recover().take()
    }

    pub fn take_recycled_scratch(&self) -> Option<ExecutionScratch> {
        self.scratch.lock_recover().take()
    }
}

impl<H: ExecutionHost> ExecutionHost for ExecutionEnvironment<'_, H> {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        self.host.perform(op).await
    }

    async fn yield_now(&self) {
        self.host.yield_now().await;
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        self.projected.clone()
    }

    fn trace_runtime_errors(&self) -> bool {
        self.trace_runtime_errors
    }

    fn profile_execution(&self) -> bool {
        self.profile_execution
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.execution_bounds
    }

    fn is_cancelled(&self) -> bool {
        self.host.is_cancelled()
    }

    fn take_scratch(&self) -> Option<ExecutionScratch> {
        self.scratch.lock_recover().take()
    }

    fn store_scratch(&self, scratch: ExecutionScratch) {
        *self.scratch.lock_recover() = Some(scratch);
    }

    fn observe_runtime_failure(&self, failure: RuntimeFailure) {
        self.host.observe_runtime_failure(failure.clone());
        *self.runtime_failure.lock_recover() = Some(failure);
    }

    fn observe_profile(&self, profile: ProfileReport) {
        self.host.observe_profile(profile.clone());
        *self.profile.lock_recover() = Some(profile);
    }

    fn observe_lashlang_execution(&self, observation: LashlangExecutionObservation) {
        self.host.observe_lashlang_execution(observation);
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[error("{message}")]
pub struct ExecutionHostError {
    message: String,
}

impl ExecutionHostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BareHost;

    impl ExecutionHost for BareHost {
        async fn perform(&self, _op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            Err(ExecutionHostError::new("no abilities"))
        }
    }

    /// A host that says nothing about memory still gets a ceiling.
    ///
    /// This is the pin on the defect, not on the number: with `Unbounded` here
    /// the heap's limit becomes `u64::MAX` and every pre-charge in the runtime
    /// — including the one that stands between `Array.from({ length })` and the
    /// OOM killer — becomes arithmetically incapable of tripping.
    #[test]
    fn the_default_host_is_memory_bounded() {
        assert!(
            matches!(
                BareHost.execution_bounds().memory_limit,
                ExecutionBound::Bounded(limit) if limit == DEFAULT_HOST_MEMORY_LIMIT_BYTES
            ),
            "the default execution bounds must carry a finite memory ceiling"
        );
    }

    /// Opting out stays possible, and stays explicit.
    #[test]
    fn a_host_can_still_declare_unbounded_memory() {
        assert!(matches!(
            ExecutionBounds::unbounded().memory_limit,
            ExecutionBound::Unbounded
        ));
    }
}
