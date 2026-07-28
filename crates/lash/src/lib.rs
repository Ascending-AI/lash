//! App-facing embedding facade for Lash.
//!
//! `lash` is intentionally a small layer above the lower-level
//! `lash-core` runtime crate. Host applications own providers, persistence,
//! app state, HTTP protocols, auth, and frontend streaming; this crate
//! owns only the ergonomic core/session/turn API.
//!
//! Every public name has exactly one home. The crate root carries the daily
//! core/session/turn path; each domain module ([`tools`], [`persistence`],
//! [`plugins`], [`observe`], [`triggers`], ...) carries its own
//! vocabulary. [`prelude`] is the curated daily-use subset of that root.

pub mod admin;
mod core;
mod error;
mod plugin_binding;
pub(crate) mod process_admin;
mod prompt_layer;
pub mod recoverable_chat;
#[cfg(feature = "rlm")]
pub mod rlm;
pub mod scenario_contracts;
mod session;
mod support;
#[cfg(all(test, feature = "rlm"))]
mod tests;
pub mod turn;
pub mod usage;

pub use crate::admin::{
    AdvancedToolAdmin, Completions, CoreTriggerAdmin, PluginOperations, SessionCommandAdmin,
    SessionTriggerAdmin, ToolAdmin,
};
pub use crate::core::{LashCore, LashCoreBuilder, SessionDeleteReport};
pub use crate::error::{EmbedError, Result};
pub use crate::plugin_binding::PluginBinding;
pub use crate::prompt_layer::PromptLayerSink;
pub use crate::session::{
    EnqueueTurnBuilder, LashSession, ObservableSession, ParkedSession, SessionBuilder,
};
pub use crate::turn::{
    QueuedTurnBuilder, TurnActivityFanout, TurnBuilder, TurnOutput, TurnResult, TurnStream,
    message_role, message_text,
};
pub use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectReplayOwnership, ExecutionSummary,
    ExternalCompletionError, GenerationOverlay, InputItem, LlmCallRecord, ModelLimits, ModelSpec,
    PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelResult,
    PendingTurnInputCancelTarget, PendingTurnInputSuffixCancelOutcome, PluginStack, Resolution,
    ResolveOutcome, SessionCommand, SessionCommandReceipt, SessionConfigPatch,
    SessionCreateRequest, SessionSpec, SessionStartPoint, TurnActivity, TurnActivityId,
    TurnActivitySink, TurnAddress, TurnAttach, TurnCancelOriginHint, TurnCancelOutcome,
    TurnCancelReceipt, TurnCancelRequest, TurnCancellationEvidence, TurnCause, TurnEvent,
    TurnFinish, TurnInput, TurnInputAcceptanceReceipt, TurnInputApplication, TurnOutcome, TurnStop,
    TurnTerminal, TurnWorkDriver,
};
/// Cooperative cancellation handle accepted by
/// [`TurnBuilder::cancel`](crate::TurnBuilder::cancel); re-exported so
/// embedders cancel turns without depending on `tokio-util` themselves.
pub use tokio_util::sync::CancellationToken;

/// `use lash::prelude::*;` brings in the daily core/session/turn vocabulary
/// without the lower-level integration types or domain modules also exposed
/// from the crate root.
pub mod prelude {
    pub use crate::{
        AdvancedToolAdmin, CoreTriggerAdmin, EmbedError, EnqueueTurnBuilder, ExecutionSummary,
        InputItem, LashCore, LashCoreBuilder, LashSession, ModelLimits, ModelSpec,
        ObservableSession, ParkedSession, PendingTurnInputCancelOutcome, PluginBinding,
        PluginOperations, PluginStack, PromptLayerSink, QueuedTurnBuilder, Result, SessionBuilder,
        SessionCommand, SessionCommandAdmin, SessionCommandReceipt, SessionConfigPatch,
        SessionCreateRequest, SessionDeleteReport, SessionSpec, SessionStartPoint,
        SessionTriggerAdmin, ToolAdmin, TurnActivity, TurnActivityFanout, TurnActivityId,
        TurnActivitySink, TurnBuilder, TurnCause, TurnEvent, TurnFinish, TurnInput,
        TurnInputAcceptanceReceipt, TurnOutcome, TurnOutput, TurnResult, TurnStop, TurnStream,
        message_role, message_text,
    };
}

/// Session observation: cursors, resumable event streams, and live replay
/// recovery for host frontends. Entry point: [`LashSession::observe`] /
/// [`ObservableSession`].
pub mod observe {
    pub use crate::session::{
        RemoteSessionObservationEventStream, RemoteSessionObservationStream,
        RemoteSessionObservationStreamItem, RemoteSessionObservationSubscription,
        SessionObservationStream, SessionObservationStreamItem,
    };
    pub use lash_core::{
        InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayGap, LiveReplayGapReason,
        LiveReplayStore, SessionCursor, SessionObservation, SessionObservationEvent,
        SessionObservationEventPayload, SessionObservationSubscription, SessionProcessEventKind,
        SessionQueueEventKind, SessionResume, SessionRevision,
    };
}

/// Triggers and subscriptions: declaring event sources, emitting occurrences,
/// and inspecting trigger subscriptions. Entry points:
/// [`LashCore::triggers`] and [`LashSession::triggers`].
pub mod triggers {
    pub use lash_core::{
        LashSchema, TriggerCommand, TriggerCommandOutcome, TriggerDeliveryEmitOutcome,
        TriggerDeliveryEmitReport, TriggerDeliveryReservation, TriggerDeliveryReservationStatus,
        TriggerEffectResult, TriggerEmitReport, TriggerEvent, TriggerEventType,
        TriggerMutationDisposition, TriggerMutationReceipt, TriggerOccurrenceFilter,
        TriggerOccurrenceRecord, TriggerOccurrenceRequest, TriggerOperationError,
        TriggerOwnerScope, TriggerRegistration, TriggerStore, TriggerSubscriptionDraft,
        TriggerSubscriptionFilter, TriggerSubscriptionRecord, TriggerTargetSummary,
        empty_trigger_source_key,
    };
}

pub mod tools {
    pub use lash_core::{
        CancelHint, PendingCompletion, PreparedToolCall, TimeoutBehavior, ToolActivation,
        ToolArgumentProjectionPolicy, ToolCall, ToolCallOutput, ToolCallRecord, ToolContext,
        ToolContract, ToolDefinition, ToolExecutionGrant, ToolFailureClass, ToolManifest,
        ToolOutputContract, ToolPrepareCall, ToolPrepareContext, ToolProvider, ToolResult,
        ToolSourceHandle, ToolTriggerClient,
    };
    pub use lash_core::{
        PLUGIN_TOOL_SOURCE_ID, ToolId, ToolRestoreReport, ToolState, ToolStateEntry,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, LASHLANG_TOOL_BINDING_KEY, LashlangToolBinding,
        RemoteToolGrantLashlangExt, ToolDefinitionLashlangExt, ToolManifestLashlangExt,
        catalogue_preview_contribution, catalogue_preview_contribution_for_entries,
        catalogue_preview_contribution_for_entries_with_options,
        catalogue_preview_contribution_for_manifests, catalogue_preview_contribution_with_options,
        catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
        catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        DeferredResolutionLinkKey, DeferredResolutionRecord, DeferredToolResolver,
        Resolution as DeferredToolResolution, SharedDeferredToolResolver,
        ToolGrant as DeferredToolGrant,
    };
    /// Author a fixed-tool provider without hand-rolling `tool_manifests` /
    /// `resolve_contract`: supply the [`ToolDefinition`]s once and an
    /// [`StaticToolExecute`] for behavior.
    pub use lash_tool_support::{StaticToolExecute, StaticToolProvider};
}

pub mod direct {
    pub use lash_core::llm::types::{
        AttachmentSource, GenerationDisposition, GenerationOptionDisposition, GenerationOptions,
        LlmEventSender, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
        NonNegativeFiniteF64, NonNegativeFiniteF64Error, ProviderFileScope,
    };
    pub use lash_core::{
        DirectCompletion, DirectJsonSchema, DirectLlmClient, DirectLlmCompletion, DirectLlmError,
        DirectLlmResult, DirectMessage, DirectOutputSpec, DirectPart, DirectRequest, DirectRole,
    };
}

pub mod persistence {
    pub use lash_core::CheckpointKind;
    pub use lash_core::FileAttachmentStore;
    pub use lash_core::runtime::{
        DeliveryPolicy, ForkPoint, ForkSessionRequest, ForkSessionResult, InMemorySessionStore,
        InMemorySessionStoreFactory, MergeKey, PendingTurnInputClaimDiagnostics,
        PendingTurnInputDraft, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
        QueuedWorkClaimBoundary, QueuedWorkClass, QueuedWorkCompletion, QueuedWorkItem,
        QueuedWorkPayload, RuntimeSessionState, SessionStoreCreateRequest, SessionStoreFactory,
        SlotPolicy, TurnInputCheckpointBoundary, TurnInputClaim, TurnInputCompletion,
        TurnInputIngress, TurnInputState,
    };
    pub use lash_core::store::queued_work;
    pub use lash_core::store::{
        GraphAppend, HydratedSessionCheckpoint, OperationId, PersistedSessionRead,
        RealizedNodeTimestamp, RuntimeCommit, RuntimeCommitResult, RuntimeTurnCommitStamp,
        SessionCheckpoint, SessionHead, SessionHeadMeta, commit_runtime_state_verified,
        load_persisted_session_state,
    };
    pub use lash_core::{
        AttachmentReclamationReport, AttachmentRootSet, AttachmentStore, InMemoryAttachmentStore,
        InMemoryProcessExecutionEnvStore, ProcessExecutionEnvStore, SessionAttachmentStore,
        StoredBlobRef, reclaim_unreferenced_attachments,
    };
    pub use lash_core::{
        BlobRef, GcReport, LeaseOwnerIdentity, LeaseOwnerLiveness, PersistedSessionConfig,
        PersistedTurnState, ProtocolEvent, QueuedWorkStore, RuntimePersistence, SessionAdmission,
        SessionBinding, SessionCommitStore, SessionExecutionLease,
        SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion,
        SessionExecutionLeaseFence, SessionExecutionLeaseStore, SessionGraph, SessionHistoryRecord,
        SessionMeta, SessionNodeRecord, SessionReadView, SessionRelation, StoreError,
        StoreMaintenance, TurnInputStore, VacuumReport,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{InMemoryLashlangArtifactStore, LashlangArtifactStore};
}

pub mod plugins {
    pub use lash_core::PluginDirective;
    pub use lash_core::PluginOptions;
    pub use lash_core::plugin::{
        AfterToolCallHook, AfterTurnHook, AssistantResponseHook, AssistantResponseHookContext,
        AssistantResponseTransform, AssistantStreamHook, AssistantStreamHookContext,
        AssistantStreamTransform, BeforeToolCallHook, BeforeTurnHook, CheckpointHook,
        CheckpointHookContext, CompactionContext, ContextCompaction, ContextCompactor,
        ContextError, PluginExtensionContribution, PluginSpecBuilder, StaticPluginFactory,
        ToolCallHookContext, ToolResultHookContext,
    };
    pub use lash_core::{
        PluginError, PluginFactory, PluginHost, PluginMessage, PluginRegistrar, PluginRuntimeEvent,
        PluginSession, PluginSessionContext, PluginSpec, PluginSpecFactory, PromptHookContext,
        SessionPlugin, ToolCatalogContribution, TurnHookContext, TurnResultHookContext,
    };
    pub use lash_plugin_tool_output_budget::{
        ToolOutputBudgetConfig, ToolOutputBudgetMode, ToolOutputBudgetPluginFactory,
        tool_output_budget_stack as runtime_plugin_stack,
    };
}

pub mod messages {
    pub use lash_core::{Message, MessageRole};
}

/// Wire-format DTOs for driving lash across a process boundary, sub-namespaced
/// by protocol domain. Only the cross-cutting handshake
/// ([`REMOTE_PROTOCOL_VERSION`](remote::REMOTE_PROTOCOL_VERSION),
/// [`ensure_protocol_version`](remote::ensure_protocol_version)) and the
/// protocol error type live at this root; everything else has exactly one
/// home in a domain sub-namespace.
pub mod remote {
    pub use lash_remote_protocol::{
        REMOTE_PROTOCOL_VERSION, RemoteProtocolError, ensure_protocol_version,
    };

    /// LLM request/response envelopes: messages, attachments, tool specs,
    /// output specs, and provider metadata.
    pub mod llm {
        pub use lash_remote_protocol::llm::{
            RemoteAttachmentRef, RemoteAttachmentSource, RemoteAttachmentTypeMetadata,
            RemoteDiagnostic, RemoteExecutionEvidence, RemoteGenerationDisposition,
            RemoteGenerationOptionDisposition, RemoteGenerationOptions, RemoteLlmContentBlock,
            RemoteLlmMessage, RemoteLlmOutputPart, RemoteLlmOutputSpec, RemoteLlmRequest,
            RemoteLlmRequestScope, RemoteLlmResponse, RemoteLlmRole, RemoteLlmTerminalReason,
            RemoteLlmToolChoice, RemoteLlmToolSpec, RemoteModelCapability, RemoteModelIntent,
            RemoteProviderFailureKind, RemoteProviderFileScope, RemoteProviderMetadata,
            RemoteProviderReasoningReplay, RemoteProviderReplayMeta, RemoteReasoningCapability,
            RemoteReasoningDisableEncoding, RemoteReasoningEncoding, RemoteReasoningSelection,
            RemoteResponseTextMeta, RemoteSchemaProjectionOverride,
        };
    }

    /// Session observation: cursors, resumable observation events, and live
    /// replay gaps.
    pub mod observations {
        pub use lash_remote_protocol::observations::{
            RemoteLiveReplayGap, RemoteLiveReplayGapReason, RemoteSessionCursor,
            RemoteSessionObservation, RemoteSessionObservationEvent,
            RemoteSessionObservationEventPayload, RemoteSessionProcessEventKind,
            RemoteSessionQueueEventKind, RemoteTurnInputApplication, RemoteTurnInputCheckpoint,
        };
    }

    /// Process lifecycle envelopes: start/cancel/signal/await/list requests
    /// and results, process records, event semantics, and execution
    /// environments.
    pub mod processes {
        pub use lash_remote_protocol::processes::{
            RemoteAbandonEvidence, RemoteAbandonRequest, RemoteAbandonWriter,
            RemoteObservedProcess, RemoteObservedProcessEvent, RemotePersistProcessEnvRequest,
            RemotePersistProcessEnvResult, RemoteProcessAwaitOutput, RemoteProcessAwaitRequest,
            RemoteProcessAwaitResult, RemoteProcessCancelRequest, RemoteProcessCancelResult,
            RemoteProcessDefinitionIdentity, RemoteProcessEvent, RemoteProcessEventSemantics,
            RemoteProcessEventSemanticsSpec, RemoteProcessEventType, RemoteProcessEventsRequest,
            RemoteProcessEventsResponse, RemoteProcessExecutionEnvRef,
            RemoteProcessExecutionEnvSpec, RemoteProcessExecutionPolicy, RemoteProcessExternalRef,
            RemoteProcessHandleDescriptor, RemoteProcessInput, RemoteProcessLifecycleStatus,
            RemoteProcessListFilter, RemoteProcessListResponse, RemoteProcessModelLimits,
            RemoteProcessModelSpec, RemoteProcessOriginator, RemoteProcessPluginOptions,
            RemoteProcessProvenance, RemoteProcessSignalRequest, RemoteProcessSignalResult,
            RemoteProcessStartGrant, RemoteProcessStartRequest, RemoteProcessStartResult,
            RemoteProcessStarted, RemoteProcessStatus, RemoteProcessStatusFilter,
            RemoteProcessSummary, RemoteProcessTerminalSemantics, RemoteProcessTerminalSpec,
            RemoteProcessTerminalState, RemoteProcessValueSelector, RemoteProcessWaitKind,
            RemoteProcessWaitState, RemoteProcessWake, RemoteProcessWakeDedupeKey,
            RemoteProcessWakeSpec, RemoteProcessWorkItem, RemoteProcessWorkSnapshot,
            RemoteRecoveryDisposition, RemoteRuntimeEffectKind, RemoteRuntimeInvocation,
            RemoteRuntimeReplay, RemoteRuntimeScope, RemoteRuntimeSubject, RemoteSessionScope,
            RemoteToolFailureClass,
        };
    }

    /// Prompt-layer envelopes: templates, slots, and contributions.
    pub mod prompt {
        pub use lash_remote_protocol::prompt::{
            RemotePromptBuiltin, RemotePromptContribution, RemotePromptContributionGate,
            RemotePromptLayer, RemotePromptSlot, RemotePromptSlotLayer, RemotePromptTemplate,
            RemotePromptTemplateEntry, RemotePromptTemplateSection,
        };
    }

    /// Tool grants and the remote tool-registry contract.
    pub mod tools {
        pub use lash_remote_protocol::registry_errors::{
            RemoteToolRegistry, assert_remote_tool_registry_reopenable,
        };
        pub use lash_remote_protocol::tools::{
            RemoteToolActivation, RemoteToolArgumentProjectionPolicy, RemoteToolGrant,
            RemoteToolOutputContract, RemoteToolRetryPolicy,
        };
    }

    /// Trigger envelopes: occurrence emission, subscriptions, and
    /// registrations.
    pub mod triggers {
        pub use lash_remote_protocol::triggers::{
            RemoteTriggerDeliveryEmitOutcome, RemoteTriggerDeliveryEmitReport,
            RemoteTriggerEmitReport, RemoteTriggerInputBinding, RemoteTriggerInputTemplate,
            RemoteTriggerListSubscriptionsResponse, RemoteTriggerOccurrenceRecord,
            RemoteTriggerOccurrenceRequest, RemoteTriggerRegisterSubscriptionRequest,
            RemoteTriggerRegisterSubscriptionResult, RemoteTriggerRegistration,
            RemoteTriggerSubscriptionDraft, RemoteTriggerSubscriptionFilter,
            RemoteTriggerSubscriptionRecord, RemoteTriggerTargetSummary,
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
            RemoteTurnCancelOutcome, RemoteTurnCancelReceipt, RemoteTurnCancelRequest,
            RemoteTurnCancellationEvidence,
        };
    }

    /// Turn result envelopes: outcomes, stops, assistant output, summaries,
    /// issues, and causal references.
    pub mod turn_result {
        pub use lash_remote_protocol::turn_result::{
            RemoteAssistantOutput, RemoteAssistantOutputState, RemoteCausalRef,
            RemoteExecutionSummary, RemoteToolCallOutcome, RemoteToolCallSummary, RemoteTurnFinish,
            RemoteTurnIssue, RemoteTurnOutcome, RemoteTurnResult, RemoteTurnStatus, RemoteTurnStop,
            RemoteTurnUsageSummary,
        };
    }

    /// Token usage accounting and the streaming turn-activity vocabulary.
    pub mod usage {
        pub use lash_remote_protocol::usage_activity::{
            RemoteTokenLedgerEntry, RemoteTurnActivity, RemoteTurnEvent, RemoteUsage,
        };
    }
}

pub mod process {
    pub use crate::admin::SessionProcessAdmin;
    pub use crate::process_admin::Processes;
    pub use lash_core::{
        AbandonEvidence, AbandonRequest, AbandonWriter, ObservedProcess, ObservedProcessEvent,
        ObservedWorkItem, ProcessAttach, ProcessAwaitOutput, ProcessAwaiter, ProcessCancelAbility,
        ProcessCancelAllRequest, ProcessCancelRequest, ProcessCancelSource, ProcessCancelSummary,
        ProcessChangeCursor, ProcessChangeHub, ProcessCompletionAuthority, ProcessEvent,
        ProcessEventAppendRequest, ProcessEventAppendResult, ProcessEventSink, ProcessEventType,
        ProcessExecutionContext, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
        ProcessExternalRef, ProcessHandleDescriptor, ProcessHandleGrant, ProcessHandleSummary,
        ProcessIdentity, ProcessInput, ProcessLease, ProcessLeaseClaimOutcome,
        ProcessLeaseCompletion, ProcessLifecycleStatus, ProcessListFilter, ProcessListMode,
        ProcessLiveReferenceSummary, ProcessOpScope, ProcessProvenance, ProcessPruneReport,
        ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessRunHandle, ProcessRuntimeHost,
        ProcessService, ProcessSessionDeleteReport, ProcessStartOptions, ProcessStartRequest,
        ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalState, ProcessWake,
        ProcessWakeDedupeKey, ProcessWakeDelivery, ProcessWakeSpec, ProcessWorkDriver,
        ProcessWorkObserver, ProcessWorkSnapshot, RecoveryDisposition, SessionScope,
        SessionScopeId, ToolSessionProcessAdmin, watch_process_registry,
        watch_process_registry_with_sink,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        LASHLANG_ENGINE_KIND, LashlangProcessInput, lashlang_process_event_types,
        lashlang_process_signal_event_types,
    };
}

pub mod durability {
    pub use lash_core::{
        DurableProcessWorker, DurableProcessWorkerConfig, EffectHost, InlineEffectHost,
        LeaseTimings, LeaseTimingsError, ProcessDrainReport, RuntimeEnvironment, RuntimeHostConfig,
        TerminationPolicy,
    };
}

pub mod runtime {
    pub use crate::core::AdvancedLashCoreBuilder;
    pub use lash_core::runtime::{
        AssembledTurn, AwaitEventResolver, DirectCompletionClient, EmbeddedRuntimeHost, EventSink,
        ExecutionScope, InlineRuntimeEffectController, LashRuntime, LlmAttachmentSpec,
        LlmRequestSpec, NoopEventSink, NoopTurnActivitySink, ProcessCommand, ProcessEffectOutcome,
        QueuedWorkDriver, QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunHandle,
        QueuedWorkRunRequest, QueuedWorkWakeDisposition, QueuedWorkWakeFailure,
        RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
        RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
        RuntimeEnvironmentBuilder, RuntimeError, RuntimeErrorCode, RuntimeHandle,
        RuntimeInvocation, RuntimeObservation, RuntimeScope, RuntimeTurnPhase,
        RuntimeTurnPhaseProbe, ScopedEffectController, TurnContext,
    };
    pub use lash_core::{
        PersistentRuntimeServices, ProtocolSessionExtensionHandle, ProtocolTurnOptions,
        SessionHandle, SessionPolicy, SessionSnapshot, render_turn_causes_prompt,
    };
}

pub mod prompt {
    pub use lash_core::{
        PromptBuiltin, PromptContribution, PromptContributionGate, PromptLayer, PromptSlot,
        PromptTemplate, PromptTemplateEntry, PromptTemplateSection, default_prompt_template,
    };
}

pub mod tracing {
    pub use lash_core::{
        JsonlTraceSink, TraceAttachment, TraceBranchSelection, TraceContentBlock,
        TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue,
        TraceError, TraceEvent, TraceLabelMetadata, TraceLlmMessage, TraceLlmRequest,
        TraceLlmResponse, TracePromptComponent, TraceProviderRequestEvent,
        TraceProviderStreamEvent, TraceRecord, TraceRuntimeScope, TraceRuntimeStreamEvent,
        TraceRuntimeSubject, TraceSinkError, TraceTokenUsage, TraceToolSpec,
    };
    #[cfg(feature = "otel-trace")]
    pub use lash_core::{OtelTraceOptions, OtelTraceSink};
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        TraceLashlangChildExecution, TraceLashlangEdgeSelection, TraceLashlangExecutionEvent,
        TraceLashlangExecutionIdentity, TraceLashlangGraph, TraceLashlangGraphChildLink,
        TraceLashlangGraphEdge, TraceLashlangGraphNode, TraceLashlangGraphStore, TraceLashlangMap,
        TraceLashlangMapEdge, TraceLashlangMapNode, TraceLashlangNodeStatus, TraceLashlangStatus,
    };
    pub use lash_trace::{StderrTraceSink, TeeTraceSink, TraceContext, TraceLevel, TraceSink};
}

/// Test helpers for embedders. Enable with `lash = { ..., features = ["testing"] }`
/// to script model responses in integration tests without a live provider.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub mod provider {
    /// Typed provider-failure classification surfaced on
    /// [`TurnIssue`](crate::turn::TurnIssue) and session error envelopes.
    pub use lash_core::ProviderFailureKind;
    pub use lash_core::provider::{
        ProviderRateLimitPolicy, ProviderReliability, ProviderRetryPolicy,
    };
    pub use lash_core::{
        CacheControlDialect, LlmTimeouts, ModelCapability, Provider, ProviderComponents,
        ProviderFactory, ProviderHandle, ProviderOptions, ProviderSpec, ReasoningCapability,
        ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection, RequestTimeout,
        SamplingCapability, StreamTermination,
    };
    /// Request/response/error vocabulary of [`Provider::complete`],
    /// re-exported so hosts can implement provider decorators (admission
    /// gates, metrics taps) against the facade alone.
    pub use lash_core::{
        ExecutionEvidence, LlmRequest, LlmRequestScope, LlmResponse, LlmTransportError,
    };
}
