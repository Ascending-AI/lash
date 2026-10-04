mod admission;
mod atomic_attempt;
mod attempt_coordinator;
mod context;
#[cfg(any(test, feature = "testing"))]
mod execution;
mod hooks;
mod intent_executor;
mod pending_resolver;
mod preparation;
mod production;
mod retry;
pub(crate) use production::ProductionToolHandlers;
mod run_coordinator;
mod singleton_run;

pub use crate::runtime::process::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, IsolatedStartRefusal,
    IsolatedToolStart, PhysicalProcessWorker, ProcessExecutionBoundary, WorkerTerminationReceipt,
};
pub use admission::{ToolRoundRefusal, admission_failure, admit_tool_round};
pub use context::{ToolDispatchContext, ToolTriggerEffectOutcome};
pub use pending_resolver::{
    ArmedResolver, LaunchReceipt, ParkSite, ResolverArming, arm_pending_resolver,
    consumer_hold_owner, discharge_abandoned_call, finish_parked_wait,
    model_visible_intent_outcomes,
};
pub use run_coordinator::{DecidedCall, RunAggregateOutcome, RunCoordinator, RunCutRefusal};
pub use singleton_run::{
    BeforeCheckReply, IsolatedProcessDescriptor, RecordedIsolatedStart, RunAttemptHandle,
    RunAttemptStep, RunRetryTimer, RunRetryWake, SingletonAttempt, SingletonBodyOutcome,
    SingletonCapture, SingletonDrift, SingletonPreparedRequest, SingletonPresentationError,
    SingletonRunError, SingletonRunOutcome, SingletonStart, SingletonTerminal, SingletonToolCall,
    SingletonToolHandlers, run_singleton_tool,
};

pub(crate) use atomic_attempt::AtomicToolAttempt;
pub use attempt_coordinator::{ToolAttemptLineage, coordinate_tool_invocation};
#[cfg(feature = "testing")]
pub use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
#[cfg(not(feature = "testing"))]
pub use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
pub use context::{
    PendingToolDispatchOutcome, ToolCallIds, ToolDispatchOutcome, ToolPreparationOutcome,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use execution::coordinate_prepared_tool_call_launch_with_execution_context;
pub use hooks::finalize_tool_result_with_execution_context;
pub(crate) use hooks::{attempt_occurrence, deferred_occurrence};
#[cfg(feature = "testing")]
pub use intent_executor::execute_final_tool_intents;
#[cfg(not(feature = "testing"))]
pub(crate) use intent_executor::execute_final_tool_intents;
#[cfg(any(test, feature = "testing"))]
pub(crate) use preparation::dispatch_tool_call_with_execution_context;
pub use preparation::resolve_callable_manifest_by_id;
#[cfg(any(test, feature = "testing"))]
pub use preparation::{dispatch_tool_call, resolve_tool_argument_projection_policy};
pub use preparation::{
    prepare_granted_tool_call_with_context, prepare_recorded_tool_call_with_context,
    prepare_tool_call_with_context, resolve_callable_manifest,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use retry::execute_once;
pub(crate) use retry::settle_completed_pending_tool_call;
pub(crate) use retry::{mark_retry_exhausted, normalized_outcome, retry_after_ms};
