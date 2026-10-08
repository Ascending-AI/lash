mod recorded_keys;
pub use recorded_keys::{RecordedKeyRange, RecordedKeys};
mod envelope;
#[doc(hidden)]
pub mod executor;
mod llm_outcome;
pub mod scope_status;
use lash_core_store::effect_identity as identity_types;
pub(crate) use tool_presentation::record_tool_presentation_plan;
mod tool_presentation;
pub use tool_presentation::{
    PresentationBinding, SessionPresentationArtifacts, ToolPresentation, retain_oversized_return,
};
mod attempt_stream;
mod request_digest;
pub use attempt_stream::{
    ATTEMPT_STREAM_BYTE_BUDGET, AttemptStream, AttemptStreamBuilder, AttemptStreamChannel,
    AttemptStreamEvent, AttemptStreamRecorder, AttemptStreamTruncation, DecodedStreamEvent,
};
mod outcome;
mod session_outcome;
mod validation;

pub use envelope::tool_cancel_work_replay_suffix;
pub use envelope::{
    AssistantResponseHookEvents, CheckpointAdmittedSet, CompactionBase, LlmRequestSpec,
    ProcessCommand, ProcessEffectOutcome, ProcessListSelection,
    RuntimeAssistantResponseHooksOutcome, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectOutcome, RuntimeInvocation,
    ServedExecutionEnvironmentSync, SleepSpec, ToolAttemptEffectOutcome, ToolAttemptLaunch,
    TurnPrelude, TurnPreludeRef, TurnPreludeStore,
};
/// Effect-executor contracts, including process local-execution capabilities.
pub use executor::{
    AdmittedDirectSend, AdmittedScope, AwaitEventKey, AwaitEventWaitIdentity, CommandJournalGuard,
    EffectJournalIdentity, EffectJournalRetirement, EffectOpener, EffectRetirementGate,
    ExecutionScope, ExternalCompletionError, JournalReplay, ProcessDefinitionLocalExecution,
    ProcessDriveStep, ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation,
    RecordedKeyFence, RefusedWriteRange, Resolution, ResolveOutcome, RuntimeEffectControllerError,
    RuntimeEffectLocalExecutor, RuntimeSleepOptions, SegmentProgress, ServedOnlyRange,
};
pub use identity_types::{
    RuntimeAttribution, RuntimeEffectKind, RuntimeReplay, RuntimeReplayAttribution, RuntimeSubject,
};
pub use lash_sansio::RunAggregateWakePolicy;
pub use lash_sansio::{CausalRef, EffectAddress};
pub use llm_outcome::{
    AssistantResponsePlan, AssistantStreamHookState, LlmStreamRecord, RuntimeLlmCallOutcome,
};
pub use validation::{
    CanonicalRuntimeEffectEnvelope, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    validate_replayed_effect_envelope,
};

pub use executor::ServedOnly;
pub use executor::TurnCancelWait;
pub use outcome::{
    LlmTraceFailure, direct_trace_context, emit_llm_trace_completed, emit_llm_trace_failed,
    emit_llm_trace_started, emit_provider_replay_drops, llm_call_error_from_transport,
};

#[cfg(test)]
mod captured_environment_row_tests {
    #[test]
    fn an_environment_load_journal_row_stays_under_the_intent_budget() {
        let policy = crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        );
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
