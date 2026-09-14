mod attempt_coordinator;
mod context;
mod directives;
mod execution;
mod intent_executor;
mod pending_resolver;
mod preparation;
mod retry;
mod scheduling;
#[cfg(test)]
mod tests;

pub use context::{ToolDispatchContext, ToolTriggerEffectOutcome};
pub(crate) use pending_resolver::arm_pending_resolver;

pub(crate) use attempt_coordinator::{
    BatchIntentDrainGate, IntentDrainGuard, ToolAttemptEffectIdentity, coordinate_tool_invocation,
};
#[cfg(feature = "testing")]
pub use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
#[cfg(not(feature = "testing"))]
pub(crate) use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
pub(crate) use context::{PendingToolDispatchOutcome, ToolDispatchOutcome, ToolPreparationOutcome};
#[cfg(any(test, feature = "testing"))]
pub use execution::coordinate_prepared_tool_call_launch_with_execution_context;
pub(crate) use execution::{
    execute_internal_process_tool, execute_orchestrating_tool,
    execute_prepared_tool_attempt_effect, finalize_tool_result_with_execution_context,
};
#[cfg(feature = "testing")]
pub use intent_executor::execute_final_tool_intents;
#[cfg(not(feature = "testing"))]
pub(crate) use intent_executor::execute_final_tool_intents;
#[cfg(test)]
pub(crate) use preparation::dispatch_tool_call;
#[cfg(test)]
pub(crate) use preparation::dispatch_tool_call_with_execution_context;
#[cfg(feature = "testing")]
pub use preparation::resolve_callable_manifest_by_id;
#[cfg(not(feature = "testing"))]
pub(crate) use preparation::resolve_callable_manifest_by_id;
#[cfg(test)]
pub(crate) use preparation::resolve_tool_argument_projection_policy;
pub(crate) use preparation::{
    prepare_granted_tool_call_with_context, prepare_tool_call_with_context,
    resolve_callable_manifest, resolve_internal_manifest_by_id,
};
#[cfg(any(test, feature = "testing"))]
pub use retry::execute_once;
pub(crate) use retry::{
    mark_retry_exhausted, normalized_outcome, resolve_retry_policy, retry_after_ms,
};
pub(crate) use scheduling::schedule_tool_batch;
