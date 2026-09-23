mod attempt_coordinator;
mod context;
mod directives;
mod execution;
mod intent_executor;
mod pending_resolver;
mod preparation;
mod retry;
#[cfg(test)]
mod tests;

pub use context::{
    REBIND_FIELDS, RebindDisposition, RebindField, TOOL_CHILD_REBIND_VERSION, ToolDispatchContext,
    ToolTriggerEffectOutcome,
};
pub use pending_resolver::arm_pending_resolver;

pub use attempt_coordinator::{
    GroupChildCoordination, ToolAttemptEffectIdentity, coordinate_tool_invocation,
};
pub use context::OrchestratingStartsBuffer;
#[cfg(feature = "testing")]
pub use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
#[cfg(not(feature = "testing"))]
pub use context::{CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer};
pub use context::{PendingToolDispatchOutcome, ToolDispatchOutcome, ToolPreparationOutcome};
#[cfg(any(test, feature = "testing"))]
pub use execution::coordinate_prepared_tool_call_launch_with_execution_context;
pub use execution::{
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
pub use preparation::resolve_callable_manifest_by_id;
#[cfg(test)]
pub(crate) use preparation::resolve_tool_argument_projection_policy;
pub use preparation::{
    prepare_granted_tool_call_with_context, prepare_tool_call_with_context,
    resolve_callable_manifest, resolve_internal_manifest_by_id,
};
#[cfg(any(test, feature = "testing"))]
pub use retry::execute_once;
pub(crate) use retry::settle_completed_pending_tool_call;
pub(crate) use retry::{
    mark_retry_exhausted, normalized_outcome, resolve_retry_policy, retry_after_ms,
};
