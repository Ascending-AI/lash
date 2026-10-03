mod recorded_keys;
pub use recorded_keys::{RecordedKeyRange, RecordedKeys};
mod envelope;
#[doc(hidden)]
pub mod executor;
mod group;
mod llm_outcome;
pub(crate) use group::await_cancelled_error;
mod group_executors;
#[cfg(any(test, feature = "testing"))]
mod layered_host;
pub mod scope_status;
use lash_core_store::effect_identity as identity_types;
mod live_openers;
pub use live_openers::{LiveOpenerContext, LiveOpenerGuard, LiveOpenerRegistry};
mod tool_child;
pub use tool_child::{
    TOOL_CHILD_REQUEST_VERSION, ToolChildAdmission, ToolChildCompletionRouting,
    ToolChildOpenerContext, ToolChildRebuildRefusal, ToolChildRequest, ToolChildScope,
    ToolChildSessionFacts, UnrecordedSessionSources,
};
pub(crate) mod tool_child_driver;
pub(crate) use tool_child_driver::runtime_ops as tool_child_runtime_ops;
#[cfg(feature = "testing")]
pub(crate) use tool_child_driver::validate_recorded_authorities;
pub use tool_child_driver::{
    ContextSourceInstall, DeploymentToolChildContext, ToolChildContextSource, ToolChildDriver,
    ToolChildHost, opener_for_execution_scope,
};
pub(crate) use tool_presentation::record_tool_presentation_plan;
mod tool_presentation;
pub use tool_presentation::{
    PresentationBinding, SessionPresentationArtifacts, TOOL_PRESENTATION_VERSION, ToolPresentation,
    retain_oversized_return,
};
mod attempt_stream;
mod request_digest;
pub use attempt_stream::{
    ATTEMPT_STREAM_BYTE_BUDGET, AttemptStream, AttemptStreamBuilder, AttemptStreamChannel,
    AttemptStreamEvent, AttemptStreamRecorder, AttemptStreamTruncation, DecodedStreamEvent,
};
pub(crate) mod tool_settlement;
pub use tool_settlement::{
    TOOL_ATTEMPT_CAPTURE_VERSION, TOOL_SETTLEMENT_VERSION, ToolAttemptCapture, ToolSettlement,
};
mod outcome;
mod shift_outcome;
pub use lash_core_effect::await_event_identity;
mod validation;

pub use crate::store::AdmittedHeadVerdict;
pub use envelope::tool_cancel_work_replay_suffix;
pub use envelope::{
    AssistantResponseHookEvents, CheckpointAdmittedSet, CompactionBase, LlmRequestSpec,
    ProcessCommand, ProcessEffectOutcome, ProcessListSelection,
    RuntimeAssistantResponseHooksOutcome, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectOutcome, RuntimeInvocation,
    ServedExecutionEnvironmentSync, SleepSpec, ToolAttemptEffectOutcome, ToolAttemptLaunch,
    ToolCompletionEvent, ToolCompletionWait, ToolDispatchCursor,
};
/// Effect-executor contracts, including process and trigger local-execution capabilities.
pub use executor::{
    AdmittedScope, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CommandJournalGuard, CompletionKeyPreparation, EffectHost, EffectJournalIdentity,
    EffectJournalRetirement, EffectOpener, EffectRetirementGate, ExecutionScope,
    ExternalCompletionError, GroupChildCancelWatch, JournalReplay, ProcessDefinitionLocalExecution,
    ProcessDriveStep, ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation,
    RecordedJournal, RecordedKeyFence, RefusedWriteRange, Resolution, ResolveOutcome,
    RunRecordStep, RuntimeAwaitEventOptions, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectLocalExecutor, RuntimeSleepOptions, ScopeBoundController, ScopedEffectController,
    SegmentProgress, ServedOnlyRange, TriggerLocalExecution, TurnCancelClosureOwnerBinding,
    TurnCancellationAuthority, TurnControlAttachment, TurnControlBinding, TurnControlBindingId,
    TurnControlBindingIdError, turn_control_binding_id_for_scope,
};
pub use group::{CommittedGroupChildFinal, EffectGroupChildCommitOutcome, GroupChildFinalCommit};
pub use group::{
    EffectGroupHandle, EffectGroupMembership, GroupChildBinding, GroupReopen, GroupSettlement,
    GroupWakePolicy, IncorporatedGroupRank, LoserPolicy, RankedGroupSettlement, RuntimeEffectGroup,
    refuse_unhonored_group_membership,
};
pub use group_executors::GroupExecutors;
pub use identity_types::{
    RuntimeAttribution, RuntimeEffectKind, RuntimeReplay, RuntimeReplayAttribution, RuntimeSubject,
};
pub use lash_sansio::{CausalRef, EffectAddress};
#[cfg(any(test, feature = "testing"))]
pub use layered_host::{EffectLayer, LayeredEffectHost};
pub use llm_outcome::{
    AssistantResponsePlan, AssistantStreamHookState, LlmStreamRecord, RuntimeLlmCallOutcome,
};
pub use validation::{
    CanonicalRuntimeEffectEnvelope, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    validate_replayed_effect_envelope,
};

pub use executor::{AdmittedProcess, EffectControllerTaskRequest, ProcessRunner, ServedOnly};
pub use executor::{
    EffectControllerTaskRequests, EffectTaskController, drive_effect_controller_task,
    effect_groups_unsupported, own_effect_controller_task,
};
pub use executor::{RUN_SEAL_OPERATION, TurnCancelWait};
pub use outcome::{
    LlmTraceFailure, direct_trace_context, emit_llm_trace_completed, emit_llm_trace_failed,
    emit_llm_trace_started, emit_provider_replay_drops, llm_call_error_from_transport,
    token_usage_from_llm,
};

#[cfg(test)]
mod captured_environment_row_tests {
    #[test]
    fn an_environment_load_journal_row_stays_under_the_intent_budget() {
        let policy =
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
        let mut plugin_config = crate::PluginConfig::for_protocol(Some("protocol".to_string()));
        plugin_config.insert(
            "protocol",
            serde_json::json!({ "prompt": { "instructions": ["x".repeat(128 * 1024)] } }),
        );
        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::new(plugin_config, 0),
            policy,
        );
        let outcome = crate::RuntimeEffectOutcome::LoadExecutionEnv {
            env: spec.stable_ref().expect("environment digest"),
        };
        let bytes = serde_json::to_vec(&outcome).expect("load journal outcome");
        assert!(
            bytes.len() < crate::TOOL_INTENT_MAX_CANONICAL_BYTES,
            "environment load journals {} bytes for 128 KiB of instructions",
            bytes.len()
        );
    }
}
