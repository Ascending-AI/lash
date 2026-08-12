use crate::{HostRequirementsRef, LashlangExecutionCallSite, ModuleRef, ProcessRef};

use super::{
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, ExecutionScratch, ProfileReport, ProjectedBindings, Record,
    RuntimeFailure, Value,
};
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
    pub results: Vec<ResourceOperationResult>,
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
}

impl ExecutionBounds {
    pub const fn new(
        instruction_budget: ExecutionBound<std::num::NonZeroU64>,
        deadline: ExecutionBound<Duration>,
    ) -> Self {
        Self {
            instruction_budget,
            deadline,
            memory_limit: ExecutionBound::instructions(DEFAULT_HEAP_LOGICAL_BYTE_LIMIT),
        }
    }

    pub const fn with_memory_limit(
        mut self,
        memory_limit: ExecutionBound<std::num::NonZeroU64>,
    ) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    pub const fn unbounded() -> Self {
        Self::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded)
            .with_memory_limit(ExecutionBound::Unbounded)
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

    fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::unbounded()
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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
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
