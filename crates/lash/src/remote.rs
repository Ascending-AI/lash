pub use lash_remote_protocol::{
    Envelope, JsonDecodeError, JsonDecodeLimits, JsonDecodeUsage, Negotiated, Negotiation,
    REMOTE_PROTOCOL, REMOTE_PROTOCOL_VERSION, RemoteProtocolError, answer,
};

/// LLM request/response envelopes: messages, attachments, tool specs,
/// output specs, and provider metadata.
pub mod llm {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_remote_protocol::{
        RemoteAttemptUsageOutcome, RemoteCacheControlDialect, RemoteProjectionMode,
        RemoteSamplingCapability, RemoteStreamTermination,
    };
    pub use lash_sansio::llm::types::{
        ChargeSafetyDecision, RetryClass, RetryDecision, RetryDeclineCause, RetryWait,
    };

    pub use lash_remote_protocol::llm::{
        RemoteAnthropicThinkingRetention, RemoteAttachmentAcceptanceRule, RemoteAttachmentAcceptor,
        RemoteAttachmentCapabilitySnapshot, RemoteAttachmentMimeSource, RemoteAttachmentRef,
        RemoteAttachmentSource, RemoteAttachmentTypeMetadata, RemoteAttemptOutcome,
        RemoteAttemptRecord, RemoteCacheRetention, RemoteDiagnostic, RemoteExecutionEvidence,
        RemoteExecutionEvidenceCollectionInterruption, RemoteGenerationOptionOutcome,
        RemoteGenerationOptions, RemoteGenerationReceipt, RemoteGoogleDialect,
        RemoteInstructionRole, RemoteLlmCallRecord, RemoteLlmContentBlock, RemoteLlmMessage,
        RemoteLlmOutputPart, RemoteLlmOutputSpec, RemoteLlmProfileCapability,
        RemoteLlmProfileRequestDefaults, RemoteLlmRequest, RemoteLlmRequestScope,
        RemoteLlmResponse, RemoteLlmRole, RemoteLlmTerminalReason, RemoteLlmToolChoice,
        RemoteLlmToolSpec, RemoteLlmTurnScope, RemoteNormalizedError, RemoteOpenAiReasoningContext,
        RemoteProtocolPosition, RemoteProviderFailureKind, RemoteProviderFileScope,
        RemoteProviderMetadata, RemoteProviderReasoningReplay, RemoteProviderReplayDrop,
        RemoteProviderReplayDropReason, RemoteProviderReplayKind, RemoteProviderReplayMeta,
        RemoteProviderRouteIdentity, RemoteReasoningCapability, RemoteReasoningEncoding,
        RemoteReasoningRetentionCapability, RemoteReasoningRetentionPolicy,
        RemoteReasoningRetentionSelection, RemoteReasoningSelection, RemoteResponseTextMeta,
        RemoteRetryClass, RemoteRetryDecision, RemoteRetryDeclineCause, RemoteRetryWait,
        RemoteSchemaContract, RemoteSchemaProjectionOverride, RemoteSchemaProjectionPolicy,
        RemoteToolResultBlock, validate_llm_request_content,
    };
}

/// Session observation: cursors, resumable observation events, and live
/// replay gaps.
pub mod observations {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_remote_protocol::{
        RemoteProcessDurableCompleteness, RemoteProcessDurableGapReason,
        RemoteProcessDurableSnapshot, RemoteProcessEffectNodeReport, RemoteProcessEffectOccurrence,
        RemoteProcessEffectOmittedCounts, RemoteProcessEffectOutcomeClass,
        RemoteProcessHistoryRetention, RemoteProcessObservationSnapshot,
    };

    pub use lash_remote_protocol::observations::{
        RemoteLiveReplayGap, RemoteLiveReplayGapReason, RemoteProcessLiveIncompleteness,
        RemoteProcessObservationCompleteness, RemoteProcessObservationGapReason,
        RemoteProcessObservationItem, RemoteProcessObservationProjection,
        RemoteProcessObservationRequest, RemoteSessionCursor, RemoteSessionObservation,
        RemoteSessionObservationEvent, RemoteSessionObservationEventPayload,
        RemoteSessionProcessEventKind, RemoteSessionQueueEventKind, RemoteTurnInputApplication,
        RemoteTurnInputCheckpoint,
    };
}

/// Process lifecycle envelopes: start/cancel/signal/await/list requests
/// and results, process records, event semantics, and execution
/// environments.
pub mod processes {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_remote_protocol::RemoteToolIntentIdentity;

    pub use lash_remote_protocol::processes::{
        RemoteAbandonEvidence, RemoteAbandonWriter, RemoteAdmittedPlugin, RemoteChargeSafetyPolicy,
        RemoteDeclaredProcessIdentity, RemoteEffectOpener, RemoteLeaseOwnerIdentity,
        RemoteLifetimeDecision, RemoteLlmProfileMetadata, RemoteModelConfig,
        RemoteNoProgressBudget, RemoteObservedProcess, RemoteObservedProcessEvent,
        RemoteObservedProcessFailure, RemoteObservedWorkItemState, RemotePersistProcessEnvReceipt,
        RemotePersistProcessEnvRequest, RemotePluginConfigNamespace, RemoteProcessAwaitOutcome,
        RemoteProcessAwaitOutput, RemoteProcessAwaitRequest, RemoteProcessCancelReceipt,
        RemoteProcessCancelRequest, RemoteProcessDefinition, RemoteProcessEvent,
        RemoteProcessEventSemantics, RemoteProcessEventSemanticsSpec, RemoteProcessEventType,
        RemoteProcessEventsRequest, RemoteProcessEventsResponse, RemoteProcessExecutionEnvRef,
        RemoteProcessExecutionEnvSpec, RemoteProcessExecutionPolicy, RemoteProcessExternalRef,
        RemoteProcessHandleView, RemoteProcessIdentity, RemoteProcessInput,
        RemoteProcessLifecycleState, RemoteProcessListFilter, RemoteProcessListResponse,
        RemoteProcessModelLimits, RemoteProcessObserverBy, RemoteProcessOriginator,
        RemoteProcessOriginatorFilter, RemoteProcessPluginConfig, RemoteProcessProvenance,
        RemoteProcessRecord, RemoteProcessResumeRefusal, RemoteProcessSignalReceipt,
        RemoteProcessSignalRequest, RemoteProcessSignalWaitBinding, RemoteProcessSignature,
        RemoteProcessStartOutcome, RemoteProcessStartReceipt, RemoteProcessStartRequest,
        RemoteProcessStartTarget, RemoteProcessStarted, RemoteProcessStatus,
        RemoteProcessStatusFilter, RemoteProcessTerminal, RemoteProcessTerminalSemantics,
        RemoteProcessTerminalSpec, RemoteProcessToolCallOutcome, RemoteProcessToolCallOutput,
        RemoteProcessToolCancellation, RemoteProcessToolFailure, RemoteProcessToolFailureSource,
        RemoteProcessValueSelector, RemoteProcessWaitKind, RemoteProcessWaitState,
        RemoteProcessWake, RemoteProcessWakeSpec, RemoteProcessWorkItem, RemoteProcessWorkSnapshot,
        RemoteRecordedRender, RemoteRetiredProcessStatus, RemoteRuntimeAttribution,
        RemoteRuntimeInvocation, RemoteRuntimeReplay, RemoteRuntimeReplayAttribution,
        RemoteRuntimeSubject, RemoteScopeGrant, RemoteScopeId, RemoteSessionScope,
        RemoteSessionTurnOutcome, RemoteStartLifetime, RemoteTerminalProcessStatus,
        RemoteToolFailureClass, RemoteTurnBudget,
    };
}

/// Tool grants and the remote tool-registry contract.
pub mod tools {
    pub use lash_remote_protocol::registry_errors::{
        RemoteToolRegistry, assert_remote_tool_registry_reopenable,
    };
    pub use lash_remote_protocol::tools::{
        RemoteExecutionPolicy, RemoteToolArgumentProjectionPolicy, RemoteToolGrant,
        RemoteToolOutputContract,
    };
}

pub mod triggers {
    pub use lash_remote_protocol::triggers::{
        RemoteTriggerDeliveryEmitOutcome, RemoteTriggerDeliveryEmitReceipt,
        RemoteTriggerDeliveryFailureCode, RemoteTriggerEmitReport, RemoteTriggerInputBinding,
        RemoteTriggerInputTemplate, RemoteTriggerListSubscriptionsResponse,
        RemoteTriggerOccurrenceOutcome, RemoteTriggerOccurrenceRecord,
        RemoteTriggerOccurrenceRequest, RemoteTriggerOwnerScope, RemoteTriggerProviderRoute,
        RemoteTriggerRegisterSubscriptionReceipt, RemoteTriggerRegisterSubscriptionRequest,
        RemoteTriggerRegistration, RemoteTriggerSourceCapture, RemoteTriggerSubscriptionDraft,
        RemoteTriggerSubscriptionFilter, RemoteTriggerSubscriptionLifecycle,
        RemoteTriggerSubscriptionRecord, RemoteTriggerSubscriptionSpec, RemoteTriggerTarget,
    };
}

/// Turn input envelopes: items, per-turn protocol options, and the turn
/// request.
pub mod turn_input {
    pub use lash_remote_protocol::turn_input::{
        RemoteInputItem, RemoteProtocolTurnOptions, RemoteTurnInput, RemoteTurnRequest,
    };
}

/// Foreground-turn cancellation request and receipt envelopes.
pub mod turn_control {
    pub use lash_remote_protocol::turn_control::{
        RemoteTurnCancelMode, RemoteTurnCancelOutcome, RemoteTurnCancelReceipt,
        RemoteTurnCancelRequest, RemoteTurnCancelUndeliveredInputPolicy,
        RemoteTurnCancellationEvidence,
    };
}

/// Turn result envelopes: outcomes, stops, assistant output, summaries,
/// issues, and causal references.
pub mod turn_result {
    pub use lash_remote_protocol::turn_result::{
        RemoteAssistantOutput, RemoteAssistantOutputState, RemoteCausalRef, RemoteOperationOutcome,
        RemoteParkedTurn, RemoteSendOutcome, RemoteStalledDelivery, RemoteToolCallOutcome,
        RemoteToolCallOutput, RemoteToolCallRecord, RemoteToolCancellation,
        RemoteToolControlProjection, RemoteToolFailure, RemoteTurnExecutionMetrics,
        RemoteTurnFinish, RemoteTurnIssue, RemoteTurnIssueSeverity, RemoteTurnOutcome,
        RemoteTurnParkReason, RemoteTurnReport, RemoteTurnStatus, RemoteTurnStop,
        RemoteTurnUsageReport,
    };
}

/// Token usage accounting and the streaming turn-activity vocabulary.
pub mod usage {
    pub use lash_remote_protocol::RemoteTurnActivitySink;
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_remote_protocol::{
        RemoteToolIntentExecutionOutcome, RemoteToolIntentKind, RemoteToolIntentRealized,
        RemoteToolIntentRefusalReason, RemoteTriggerMutationReceipt,
    };

    pub use lash_remote_protocol::queued_events::{
        RemoteAdmissionBoundary, RemoteMessageOrigin, RemoteMessageRole, RemotePart,
        RemotePartAttachment, RemotePartKind, RemotePluginMessage, RemoteTurnCause,
        RemoteTurnOutputSource,
    };
    pub use lash_remote_protocol::usage_activity::{
        RemoteTurnActivity, RemoteTurnEvent, RemoteUsage,
    };
}
