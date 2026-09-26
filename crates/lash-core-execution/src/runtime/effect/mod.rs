pub mod await_event_coordinator;
pub mod effect_replay_driver;
pub use effect_replay_driver::{RecordedKeyRange, RecordedKeys};
mod envelope;
#[doc(hidden)]
pub mod executor;
mod group;
pub(crate) use group::await_cancelled_error;
pub mod group_closing;
pub mod group_drain;
mod group_journal;
#[cfg(any(test, feature = "testing"))]
mod layered_host;
pub mod scope_status;
use lash_core_store::effect_identity as identity_types;
mod live_openers;
pub use live_openers::{LiveOpenerContext, LiveOpenerGuard, LiveOpenerRegistry};
mod tool_child;
pub use tool_child::{
    TOOL_CHILD_REQUEST_VERSION, ToolChildAdmission, ToolChildCompletionRouting,
    ToolChildRebuildRefusal, ToolChildRequest, ToolChildScope, ToolChildSessionFacts,
    UnrecordedSessionSources,
};
mod tool_child_driver;
pub(crate) use tool_child_driver::await_journaled_tool_completion;
#[cfg(feature = "testing")]
pub(crate) use tool_child_driver::validate_recorded_authorities;
pub use tool_child_driver::{
    ContextSourceInstall, DeploymentToolChildContext, ToolChildContextSource, ToolChildDriver,
    ToolChildHost, opener_for_execution_scope,
};
mod tool_presentation;
pub use tool_presentation::{
    SessionPresentationArtifacts, TOOL_PRESENTATION_VERSION, ToolPresentation,
};
mod recorded_stream;
pub use recorded_stream::{
    CHILD_STREAM_BYTE_BUDGET, ChildStreamTruncation, DecodedChildEvent, RecordedChildChannel,
    RecordedChildEvent, RecordedChildStream,
};
mod tool_settlement;
pub use tool_settlement::{
    TOOL_ATTEMPT_CAPTURE_VERSION, TOOL_SETTLEMENT_VERSION, ToolAttemptCapture, ToolSettlement,
    ToolUsageDelta, ToolUsageLedger,
};
mod drive_outcome;
mod outcome;
pub use lash_core_effect::promise_semantics;
mod validation;

pub use envelope::{
    AssistantResponseHookEvents, AssistantStreamHookState, CheckpointClaimSet, LlmRequestSpec,
    LlmStreamRecord, ProcessCommand, ProcessEffectOutcome, RuntimeAssistantResponseHooksOutcome,
    RuntimeDirectLlmOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectOutcome, RuntimeInvocation, RuntimeLlmCallOutcome, ServedExecutionEnvironmentSync,
    SleepSpec, ToolAttemptEffectOutcome, ToolAttemptLaunch, ToolInvocationEffectOutcome,
};
/// Effect-executor contracts, including process and trigger local-execution capabilities.
pub use executor::{
    AdmittedScope, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CommandJournalGuard, CompletionKeyPreparation, EffectHost, EffectJournalIdentity,
    EffectJournalRetirement, EffectOpener, EffectRetirementGate, ExecutionScope,
    ExternalCompletionError, IndependentEffectWork, ProcessDriveStep, ProcessLocalExecution,
    ProcessOutcomeObserver, ProcessTurnCancellation, QueuedLaneAcquisition, QueuedLaneAttempt,
    QueuedLaneGuard, QueuedLaneHolder, QueuedLaneProbe, RecordedJournal, RecordedKeyFence,
    RefusedWriteRange, Resolution, ResolveOutcome, RuntimeAwaitEventOptions,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectLocalExecutor,
    RuntimeSleepOptions, ScopeBoundController, ScopedEffectController, SegmentProgress,
    ServedOnlyRange, ToolIntentOutcomeSink, ToolIntentPreparation, ToolIntentSubmissionGuard,
    TriggerLocalExecution, TurnCancelClosureOwnerBinding, TurnCancellationAuthority,
    TurnControlAttachment, TurnControlBinding, TurnControlBindingId, TurnControlBindingIdError,
    turn_control_binding_id_for_scope,
};
pub use group::{
    EffectGroupDrainBudget, EffectGroupHandle, EffectGroupMembership, GroupChildBinding,
    GroupReopen, GroupSettlement, GroupWakePolicy, IncorporatedGroupRank, LoserPolicy,
    RankedGroupSettlement, RuntimeEffectGroup, refuse_unhonored_group_membership,
};
pub use group_closing::{
    GroupFinalizationReport, GroupOnlyFinalization, OpenerFinalizationSteps,
    StoreEffectGroupClosing, UnsettledEffectGroup,
};
pub use group_drain::{
    ChildDrainOutcome, DrainedChild, GroupDrainReport, GroupExecutors, StoreEffectGroupDrain,
};
pub use group_journal::{EffectGroupChildCommitOutcome, GroupChildFinalCommit};
pub use identity_types::{
    RuntimeAttribution, RuntimeEffectKind, RuntimeReplay, RuntimeReplayAttribution, RuntimeSubject,
};
pub use lash_sansio::{CausalRef, EffectAddress};
#[cfg(any(test, feature = "testing"))]
pub use layered_host::{EffectLayer, LayeredEffectHost};
pub use validation::{
    CanonicalRuntimeEffectEnvelope, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    validate_replayed_effect_envelope,
};

pub use executor::{AdmittedProcess, EffectControllerTaskRequest, ProcessRunner, ServedOnly};
pub use executor::{EffectTaskController, drive_effect_controller_task, effect_groups_unsupported};
pub use executor::{RUN_SEAL_OPERATION, RuntimeEffectControllerHandle, TurnCancelWait};
pub use outcome::{
    LlmTraceFailure, direct_trace_context, emit_llm_trace_completed, emit_llm_trace_failed,
    emit_llm_trace_started, emit_provider_replay_drops, llm_call_error_from_transport,
    token_usage_from_llm,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_effect_envelope_round_trips_prepared_tool_call() {
        let registration = crate::ProcessRegistration::new(
            crate::ProcessInput::ToolCall {
                call: crate::PreparedToolCall {
                    call_id: "call-123".to_string(),
                    tool_id: crate::ToolId::from("tool:echo"),
                    tool_name: "echo".to_string(),
                    args: serde_json::json!({"value": "hi"}),
                    replay: None,
                    prepared_payload: serde_json::json!({"context": "prepared"}),
                },
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::turn("session", "turn"),
                "session:turn:process:start:call-123",
            )
            .expect("valid process envelope address"),
            RuntimeAttribution::for_turn("session", "turn", 0, 0),
            "process:start:call-123",
        );
        let envelope = RuntimeEffectEnvelope::new(
            invocation,
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration,
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        );

        let hash = envelope.stable_hash().expect("hash");
        let decoded: RuntimeEffectEnvelope =
            serde_json::from_str(&serde_json::to_string(&envelope).expect("serialize"))
                .expect("decode");

        assert_eq!(decoded.command.kind(), RuntimeEffectKind::Process);
        assert_eq!(decoded.stable_hash().expect("decoded hash"), hash);
        let RuntimeEffectCommand::Process { command } = decoded.command else {
            panic!("wrong process command");
        };
        let ProcessCommand::Start {
            registration,
            observers,
            execution_context,
            ..
        } = *command
        else {
            panic!("wrong process command");
        };
        assert!(observers.is_empty());
        assert!(execution_context.is_empty());
        let crate::ProcessInput::ToolCall { call } = registration.input.as_ref() else {
            panic!("wrong process input");
        };
        assert_eq!(call.call_id, "call-123");
        assert_eq!(call.tool_name, "echo");
        assert_eq!(call.args, serde_json::json!({"value": "hi"}));
        assert_eq!(
            call.prepared_payload,
            serde_json::json!({"context": "prepared"})
        );
    }

    fn prepared_tool_call(call_id: &str, tool_name: &str) -> crate::PreparedToolCall {
        crate::PreparedToolCall {
            call_id: call_id.to_string(),
            tool_id: crate::ToolId::from(format!("tool:{tool_name}")),
            tool_name: tool_name.to_string(),
            args: serde_json::json!({"value": call_id}),
            replay: None,
            prepared_payload: serde_json::json!({"prepared": true}),
        }
    }

    /// A group child's attempt envelopes hash from its replay suffix, so the
    /// suffix a prepared batch derives is identity material (ADR 0099 §3).
    #[test]
    fn prepared_tool_batch_derives_positional_child_replay_suffixes() {
        let batch = crate::PreparedToolBatch::new(
            "batch-123",
            vec![
                prepared_tool_call("call-1", "echo"),
                prepared_tool_call("call-2", "lookup"),
            ],
        );
        assert_eq!(batch.batch_id, "batch-123");
        assert_eq!(batch.calls.len(), 2);
        assert_eq!(batch.calls[0].call.call_id, "call-1");
        assert_eq!(batch.calls[0].replay_suffix, "child:0:call-1");
        assert_eq!(batch.calls[1].call.call_id, "call-2");
        assert_eq!(batch.calls[1].replay_suffix, "child:1:call-2");
    }
}
