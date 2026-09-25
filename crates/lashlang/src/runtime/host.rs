use crate::{
    HostRequirementsRef, LashlangEffectFailure, LashlangExecutionCallSite, ModuleRef, ProcessRef,
};

use super::{
    ExecutionScratch, ProfileReport, ProjectedBindings, Record, RuntimeFailure, Value,
    error::ExecutionHostToolFailure,
};
use crate::LashlangExecutionObservation;
use lash_sansio::{
    ToolFailure, ToolFailureClass, ToolFailureSource, ToolRetryStatus, sync::MutexExt,
};
use std::future::Future;
use std::sync::Mutex;
use thiserror::Error;

#[derive(Clone, Debug)]
pub enum AbilityOp {
    /// Boxed: a resource operation carries a receiver value, its arguments and
    /// a call site, and it is several times the size of every other ability.
    /// Inlining it would make every `AbilityOp` that large.
    ResourceOperation(Box<ResourceOperation>),
    ResourceOperationBatch(ResourceOperationBatch),
    Await(Value),
    Print(Value),
    Finish(Value),
    Fail(Value),
    ProcessEvent(ProcessEvent),
    Sleep(Sleep),
    WaitSignal {
        name: String,
        call_site: Option<LashlangExecutionCallSite>,
    },
}

#[derive(Clone, Debug)]
pub enum AbilityResult {
    Value(Value),
    ResourceOperationBatch(ResourceOperationBatchResult),
    Unit,
}

impl AbilityResult {
    /// Takes the host's value, refusing one that carries a prototype-chain name
    /// as a data key.
    ///
    /// A host result is the second way such a key can enter — a tool result, an
    /// awaited process result, a signal payload — and this is the seam every
    /// ability's value passes through. See
    /// `access::prototype_chain_data_key_error` for why entry rather than the
    /// first read is where it is refused.
    pub fn into_value(self, op: &'static str) -> Result<Value, ExecutionHostError> {
        match self {
            Self::Value(value) => {
                match crate::runtime::access::prototype_chain_data_key_error(&value) {
                    Some(error) => Err(ExecutionHostError::new(format!("{op} returned {error}"))),
                    None => Ok(value),
                }
            }
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
    /// One entry per **unique** pending operation, numbered in the order each
    /// first appears in the operand. A handle written at two positions is one
    /// leaf (ADR 0099 §10 L4, §11 clause 1): execution deduplicates, input
    /// positions never do, and the VM expands a leaf's outcome to every
    /// position that names it.
    pub leaves: Vec<ResourceOperationBatchLeaf>,
    /// How the aggregate consumes its leaves' settlements (ADR 0099 §10 L1).
    /// A caller-side loop decision, never journaled; the host derives the
    /// journaled wake policy from it.
    pub consumer: AggregateConsumer,
    /// For `race` and `any`: how many leaves precede, in source order, the
    /// first operand that is already a plain value. That value is part of the
    /// immediate prefix (§10 L5): it decides the aggregate unless an earlier
    /// leaf settled during preparation does, and every pending leaf is still
    /// admitted before it answers (§11 clause 3). `None` when no plain value
    /// is present, and always `None` for the all-results consumers.
    pub settled_value_after: Option<usize>,
}

#[cfg(any(test, feature = "testing"))]
impl ResourceOperationBatch {
    /// A scripted host's answer: its leaves settle in leaf order and none
    /// during preparation, as a host that resolved them one after another
    /// would report. `results` holds one result per leaf, in leaf order.
    ///
    /// Testing only: the reply algebra is the host's to derive from the
    /// settlements it actually observed. This helper synthesizes a selection
    /// from results a test already holds, which no product host may do
    /// (ADR 0099 §10 L2, L6).
    ///
    /// A plain operand is part of the immediate prefix, which answers ahead of
    /// every dispatched settlement (ADR 0099 §10 L5), so such a host answers a
    /// `race` or an `any` that holds one with [`ResourceOperationBatchResult::SettledValue`].
    #[must_use]
    pub fn answer_in_leaf_order(
        &self,
        results: Vec<ResourceOperationResult>,
    ) -> ResourceOperationBatchResult {
        let first_rejection = || {
            results
                .iter()
                .position(|result| matches!(result, ResourceOperationResult::Error(_)))
        };
        match self.consumer {
            AggregateConsumer::AllSettled => ResourceOperationBatchResult::AllResults(results),
            AggregateConsumer::All => match first_rejection() {
                Some(leaf) => ResourceOperationBatchResult::Selected {
                    leaf,
                    result: results[leaf].clone(),
                },
                None => ResourceOperationBatchResult::AllResults(results),
            },
            AggregateConsumer::Race | AggregateConsumer::Any
                if self.settled_value_after.is_some() =>
            {
                ResourceOperationBatchResult::SettledValue
            }
            AggregateConsumer::Race => match results.into_iter().next() {
                Some(result) => ResourceOperationBatchResult::Selected { leaf: 0, result },
                None => ResourceOperationBatchResult::AllResults(Vec::new()),
            },
            AggregateConsumer::Any => {
                match results
                    .iter()
                    .position(|result| matches!(result, ResourceOperationResult::Value(_)))
                {
                    Some(leaf) => ResourceOperationBatchResult::Selected {
                        leaf,
                        result: results[leaf].clone(),
                    },
                    None => ResourceOperationBatchResult::ExhaustedRejections(
                        results
                            .into_iter()
                            .filter_map(|result| match result {
                                ResourceOperationResult::Error(error) => Some(error),
                                ResourceOperationResult::Value(_) => None,
                            })
                            .collect(),
                    ),
                }
            }
        }
    }
}

/// One unique pending operation of an aggregate.
#[derive(Clone, Debug)]
pub enum ResourceOperationBatchLeaf {
    /// A resource (tool) operation.
    Operation(ResourceOperation),
    /// A timer from an unawaited `sleep(ms)`. Its start point is its
    /// admission and its fulfilment value is `undefined` (ADR 0099 §11
    /// clause 4); the host records the deadline once when it admits the
    /// aggregate.
    Timer(Sleep),
}

impl ResourceOperationBatchLeaf {
    /// The resource operation, when this leaf is one.
    #[must_use]
    pub fn operation(&self) -> Option<&ResourceOperation> {
        match self {
            Self::Operation(operation) => Some(operation),
            Self::Timer(_) => None,
        }
    }
}

/// How an aggregate consumes its leaves' settlements (ADR 0099 §10 L1).
///
/// Four modes over three journaled wake policies: `all` and `allSettled` ask
/// the host for the same thing and differ only in how far the caller consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateConsumer {
    /// Every leaf's result, in leaf order. `Promise.allSettled`, and every
    /// Lashlang-native aggregate: those wait for all results and report the
    /// first *written* rejection (§10 L7).
    AllSettled,
    /// `Promise.all`: the first consumed rejection, or every result when
    /// none rejects.
    All,
    /// `Promise.race`: the first settlement.
    Race,
    /// `Promise.any`: the first fulfilment, or every rejection.
    Any,
}

impl AggregateConsumer {
    /// The consumer a TypeScript `Promise.<method>` aggregate lowers to, by
    /// the method's name: `all`, `allSettled`, `race` or `any`.
    #[must_use]
    pub fn from_typescript_method(method: &str) -> Option<Self> {
        match method {
            "all" => Some(Self::All),
            "allSettled" => Some(Self::AllSettled),
            "race" => Some(Self::Race),
            "any" => Some(Self::Any),
            _ => None,
        }
    }
}

/// The host's answer to an aggregate — the total response algebra of ADR 0099
/// §10 L2. Infrastructure failure and host cancellation are not in it: they
/// travel as the ability's `Err` (L3) and never become a leaf rejection.
#[derive(Clone, Debug)]
pub enum ResourceOperationBatchResult {
    /// One result per leaf, in leaf order: `allSettled`, a successful `all`,
    /// and every Lashlang-native aggregate.
    AllResults(Vec<ResourceOperationResult>),
    /// The one settlement that decided the aggregate: the first settlement
    /// for `race`, the first fulfilment for `any`, the first rejection for
    /// `all`. `leaf` indexes [`ResourceOperationBatch::leaves`].
    Selected {
        leaf: usize,
        result: ResourceOperationResult,
    },
    /// The plain value [`ResourceOperationBatch::settled_value_after`] names
    /// decided a `race` or `any`; every pending leaf was admitted first.
    SettledValue,
    /// `any` with no fulfilment: each leaf's rejection, in leaf order. The VM
    /// expands them to input positions, duplicates included (§10 L2, §11
    /// clause 8).
    ExhaustedRejections(Vec<ExecutionHostError>),
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
    pub call_site: Option<LashlangExecutionCallSite>,
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
    /// # Panics
    ///
    /// Panics when `instructions` is zero.
    pub const fn instructions(instructions: u64) -> Self {
        match std::num::NonZeroU64::new(instructions) {
            Some(instructions) => Self::Bounded(instructions),
            None => panic!("instruction budget must be non-zero"),
        }
    }

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

/// Independent limits for active Lashlang VM execution.
///
/// Foreground executions receive fresh meters for each block. Durable process
/// executions persist both meters in every continuation, so the limits are
/// cumulative across segment handovers for the process's entire life.
/// Enforcement occurs after intrinsic dispatch, before and after effects, at
/// cooperative yields, and at terminal VM exits, so instruction limits can
/// overshoot only by one bounded dispatch/check interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub instruction_budget: ExecutionBound<std::num::NonZeroU64>,
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
    /// Both limits are stated: a host that does not decide how much
    /// logical memory an execution may hold has not finished configuring it,
    /// and a silent default here would be a bound nobody chose.
    pub const fn new(
        instruction_budget: ExecutionBound<std::num::NonZeroU64>,
        memory_limit: ExecutionBound<std::num::NonZeroU64>,
    ) -> Self {
        Self {
            instruction_budget,
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

    /// Unbounded instructions with the default memory ceiling: what a
    /// host gets when it does not override `execution_bounds`.
    pub const fn memory_bounded_default() -> Self {
        Self::new(
            ExecutionBound::Unbounded,
            ExecutionBound::Bounded(DEFAULT_HOST_MEMORY_LIMIT_BYTES),
        )
    }

    pub const fn unbounded() -> Self {
        Self::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded)
    }
}

pub trait ExecutionHost: Sync {
    fn perform(
        &self,
        op: AbilityOp,
    ) -> impl Future<Output = Result<AbilityResult, ExecutionHostError>> + Send;

    /// The run's cancel checkpoint: the VM awaits it each time its
    /// executed-instruction count crosses a multiple of
    /// [`CANCEL_CHECKPOINT_INSTRUCTIONS`](crate::CANCEL_CHECKPOINT_INSTRUCTIONS),
    /// numbering them from one, and then consults
    /// [`is_cancelled`](Self::is_cancelled). A host whose cancellation is an
    /// engine event answers it with a recorded operation, which is also the
    /// run's only wait on a long stretch of pure compute; the default answers
    /// nothing. A checkpoint never decides anything by itself.
    fn cancel_checkpoint(&self, checkpoint: u64) -> impl Future<Output = ()> + Send {
        let _ = checkpoint;
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

    /// Whether the VM delivers Lashlang execution observations to
    /// [`Self::observe_lashlang_execution`]. The VM builds none for a host
    /// that answers `false`, which keeps loop steps, branches and calls free
    /// of the work; a host that observes must answer `true`.
    fn observes_lashlang_execution(&self) -> bool {
        false
    }

    /// Receives each Lashlang execution observation, when
    /// [`Self::observes_lashlang_execution`] answers `true`.
    fn observe_lashlang_execution(&self, _observation: LashlangExecutionObservation) {}
}

pub struct ExecutionEnvironment<'host, H: ExecutionHost> {
    host: &'host H,
    mode: ExecutionMode,
    projected: ProjectedBindings,
    scratch: Mutex<Option<ExecutionScratch>>,
    trace_runtime_errors: bool,
    profile_execution: bool,
    observes_lashlang_execution: bool,
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
            observes_lashlang_execution: host.observes_lashlang_execution(),
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

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.host.cancel_checkpoint(checkpoint).await;
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

    fn observes_lashlang_execution(&self) -> bool {
        self.observes_lashlang_execution
    }

    fn observe_lashlang_execution(&self, observation: LashlangExecutionObservation) {
        self.host.observe_lashlang_execution(observation);
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[error("{message}")]
pub struct ExecutionHostError {
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_failure: Option<Box<ExecutionHostToolFailure>>,
}

impl ExecutionHostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            tool_failure: None,
        }
    }

    /// Preserves the guest-observable classification of a failed tool call.
    ///
    /// The optional raw tool payload is deliberately not promoted into the
    /// execution-host error contract. Callers can inspect the stable failure
    /// classification without treating foreign JSON as structured control
    /// data.
    pub fn from_tool_failure(failure: &ToolFailure, replay_key: impl Into<String>) -> Self {
        Self {
            message: failure.message.clone(),
            tool_failure: Some(Box::new(ExecutionHostToolFailure {
                class: failure.class.clone(),
                code: failure.code.clone(),
                source: failure.source.clone(),
                retry: failure.retry.clone(),
                replay_key: replay_key.into(),
            })),
        }
    }

    /// The recorded tool failure attached to this host error, if any.
    pub fn tool_failure(&self) -> Option<LashlangEffectFailure> {
        let failure = self.tool_failure.as_ref()?;
        Some(LashlangEffectFailure {
            class: failure.class.clone(),
            code: failure.code.clone(),
            message: self.message.clone(),
            replay_key: failure.replay_key.clone(),
            source: failure.source.clone(),
            retry: failure.retry.clone(),
        })
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn tool_failure_class(&self) -> Option<&ToolFailureClass> {
        self.tool_failure.as_ref().map(|failure| &failure.class)
    }

    pub fn tool_failure_code(&self) -> Option<&str> {
        self.tool_failure
            .as_ref()
            .map(|failure| failure.code.as_str())
    }

    pub fn tool_failure_source(&self) -> Option<&ToolFailureSource> {
        self.tool_failure.as_ref().map(|failure| &failure.source)
    }

    /// Returns the tool retry disposition when this error crossed a tool bridge.
    pub fn tool_failure_retry(&self) -> Option<&ToolRetryStatus> {
        self.tool_failure.as_ref().map(|failure| &failure.retry)
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

    #[test]
    fn plain_execution_host_errors_keep_the_message_only_contract() {
        let error = ExecutionHostError::new("plain host failure");
        assert_eq!(error.message(), "plain host failure");
        assert_eq!(error.tool_failure_class(), None);
        assert_eq!(error.tool_failure_code(), None);
        assert_eq!(error.tool_failure_source(), None);
        assert_eq!(error.tool_failure_retry(), None);
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::json!({ "message": "plain host failure" })
        );
        assert_eq!(
            serde_json::from_value::<ExecutionHostError>(serde_json::json!({
                "message": "plain host failure"
            }))
            .unwrap(),
            error
        );
    }
}
