mod attempt_coordinator;
mod context;
mod directives;
mod execution;
mod intent_executor;
mod preparation;
mod retry;
mod scheduling;
#[cfg(test)]
mod tests;

pub use context::{ToolDispatchContext, ToolTriggerEffectOutcome};

pub(crate) use attempt_coordinator::{
    BatchIntentDrainGate, IntentDrainSlot, ToolAttemptEffectIdentity, coordinate_tool_invocation,
};
pub(crate) use context::{
    CheckpointMessageBuffer, PendingToolDispatchOutcome, RecordedToolIntentOutcomeBuffer,
    ToolCallLaunch, ToolDispatchOutcome, ToolPreparationOutcome, ToolTriggerOutcomeBuffer,
};
#[cfg(test)]
pub(crate) use execution::coordinate_prepared_tool_call_launch_with_execution_context;
pub(crate) use execution::{
    execute_internal_process_tool, execute_orchestrating_tool,
    execute_prepared_tool_attempt_effect, finalize_tool_result_with_execution_context,
};
pub(crate) use intent_executor::{execute_final_tool_intents, execute_parent_end_actions};
#[cfg(test)]
pub(crate) use preparation::dispatch_tool_call;
#[cfg(test)]
pub(crate) use preparation::dispatch_tool_call_with_execution_context;
#[cfg(test)]
pub(crate) use preparation::resolve_tool_argument_projection_policy;
pub(crate) use preparation::{
    prepare_granted_tool_call_with_context, prepare_tool_call_with_context,
    resolve_callable_manifest, resolve_callable_manifest_by_id, resolve_internal_manifest_by_id,
};
#[cfg(test)]
pub(crate) use retry::execute_once;
pub(crate) use retry::{mark_retry_exhausted, retry_after_ms};
pub(crate) use scheduling::schedule_tool_batch;
