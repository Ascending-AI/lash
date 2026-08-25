//! App-facing embedding facade for Lash.
//!
//! `lash` is intentionally a small layer above the lower-level
//! `lash-core` runtime crate. Host applications own providers, persistence,
//! app state, HTTP protocols, auth, and frontend streaming; this crate
//! owns only the ergonomic core/session/turn API.
//!
//! Every public name has exactly one home. The crate root carries the daily
//! core/session/turn path; each domain module ([`tools`], [`persistence`],
//! [`plugins`], [`observe`], [`triggers`], [`attachments`], ...) carries its own
//! vocabulary. [`prelude`] is the curated daily-use subset of that root.

#![deny(missing_docs)]

/// Administrative facade handles and operations.
pub mod admin;
mod core;
mod error;
pub mod formats;
mod plugin_binding;
pub mod preflight;
pub(crate) mod process_admin;
mod prompt_layer;
pub mod recoverable_chat;
#[cfg(feature = "rlm")]
/// RLM-specific turn-builder extensions.
pub mod rlm;
/// Reusable contracts for agent scenarios.
pub mod scenario_contracts;
/// Standard-lock poison recovery traits for application code.
pub mod sync {
    pub use lash_core::sync::*;
}
mod session;
mod session_lease;
mod support;
#[cfg(test)]
mod tests;
mod tool_catalog;
mod tool_intent_ingress;
/// Turn builders, streams, activities, and output types.
pub mod turn;
pub mod usage;

pub use crate::admin::{
    AdvancedToolAdmin, Completions, CoreTriggerAdmin, PluginOperations, SessionCommandAdmin,
    SessionTriggerAdmin, ToolAdmin,
};
pub use crate::core::{DeploymentDrainStatus, LashCore, LashCoreBuilder, SessionDeleteReport};
pub use crate::error::{EmbedError, Result, SelectedQueuedWorkDrainRefusalCause};
pub use crate::plugin_binding::PluginBinding;
pub use crate::prompt_layer::PromptLayerSink;
pub use crate::session::{
    EnqueueTurnBuilder, LashSession, ObservableSession, ParkedSession, SessionBuilder,
};
pub use crate::tool_catalog::{ToolCatalogMiss, ToolCatalogView};
pub use crate::turn::queued_drain::{EmptyQueuedDrainReason, QueuedTurnDrain};
pub use crate::turn::{
    QueuedTurnBuilder, SelectedQueuedTurnBuilder, SelectedQueuedWorkBatchSatisfaction,
    SelectedQueuedWorkDrainOutcome, TurnActivityFanout, TurnBuilder, TurnOutput, TurnReport,
    TurnStream, message_role, message_text,
};
pub use lash_core::facade_support::{
    TurnCancelAffectedInput, TurnCancelDisposition, TurnCancelInputOutcome, TurnCancelRequestRecord,
};
pub use lash_core::runtime::ExternalCompletionError;
pub use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, CommitBudget, CommitBudgetLimit, DrainMode,
    DrainModePolicy, EffectReplayOwnership, FrameKey, InputItem, LlmCallRecord, ModelLimits,
    ModelLimitsError, ModelSpec, ModelSpecBuilder, NoProgressBudget, PendingTurnInput,
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputSuffixCancelOutcome, QueuedDrainCandidate, QueuedDrainPolicy,
    QueuedDrainRequest, QueuedDrainSelection, QueuedWorkBatchingConfig, QueuedWorkClaimRefusal,
    Resolution, ResolveOutcome, SessionCreateRequest, SessionError, SessionListFilter,
    SessionRelationKind, SessionStartPoint, SessionSummary, TurnActivity, TurnActivityId,
    TurnBudget, TurnCancelOriginHint, TurnCause, TurnEvent, TurnInput, TurnInputApplication,
    facade_support::GenerationOverlay, facade_support::PluginStack, facade_support::SessionCommand,
    facade_support::SessionCommandReceipt, facade_support::SessionConfigPatch,
    facade_support::SessionSpec, facade_support::TurnActivitySink, facade_support::TurnAddress,
    facade_support::TurnAttach, facade_support::TurnCancelOutcome,
    facade_support::TurnCancelReceipt, facade_support::TurnCancelRequest,
    facade_support::TurnCancellationEvidence, facade_support::TurnExecutionMetrics,
    facade_support::TurnFinish, facade_support::TurnInputAcceptanceReceipt,
    facade_support::TurnOutcome, facade_support::TurnStop, facade_support::TurnTerminal,
    facade_support::TurnWorkDriver, facade_support::WorkerSlotKind,
    facade_support::WorkerSlotPermit, facade_support::WorkerSlotSupplier,
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
        AdvancedToolAdmin, CoreTriggerAdmin, DeploymentDrainStatus, EmbedError, EnqueueTurnBuilder,
        InputItem, LashCore, LashCoreBuilder, LashSession, ModelLimits, ModelLimitsError,
        ModelSpec, ModelSpecBuilder, NoProgressBudget, ObservableSession, ParkedSession,
        PendingTurnInputCancelOutcome, PluginBinding, PluginOperations, PluginStack,
        PromptLayerSink, QueuedTurnBuilder, Result, SessionBuilder, SessionCommand,
        SessionCommandAdmin, SessionCommandReceipt, SessionConfigPatch, SessionCreateRequest,
        SessionDeleteReport, SessionListFilter, SessionRelationKind, SessionSpec,
        SessionStartPoint, SessionSummary, SessionTriggerAdmin, ToolAdmin, TurnActivity,
        TurnActivityFanout, TurnActivityId, TurnActivitySink, TurnBudget, TurnBuilder, TurnCause,
        TurnEvent, TurnExecutionMetrics, TurnFinish, TurnInput, TurnInputAcceptanceReceipt,
        TurnOutcome, TurnOutput, TurnReport, TurnStop, TurnStream, message_role, message_text,
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
        LiveReplayEventDraft, LiveReplayGapReason, LiveReplayStore, LiveReplayStoreError,
        LiveReplaySubscribeOutcome, PreparedLiveReplayPublication, SessionCursor,
        SessionObservationEvent, SessionObservationEventPayload, SessionProcessEventKind,
        SessionQueueEventKind, SessionRevision, facade_support::InMemoryLiveReplayStore,
        facade_support::InMemoryLiveReplayStoreConfig, facade_support::LiveReplayGap,
        facade_support::SessionObservation, facade_support::SessionObservationSubscription,
        facade_support::SessionResume,
    };
}

/// Triggers and subscriptions: declaring event sources, emitting occurrences,
/// and inspecting trigger subscriptions. Entry points:
/// [`LashCore::triggers`] and [`LashSession::triggers`].
///
/// Reads have a facade: [`CoreTriggerAdmin::subscriptions`](crate::admin::CoreTriggerAdmin::subscriptions)
/// and [`SessionTriggerAdmin`] project registrations for host and session
/// scopes. Mutations go through the store contract below:
/// [`TriggerCommand`](crate::triggers::TriggerCommand) executed by
/// [`TriggerStore::execute_command`](crate::triggers::TriggerStore::execute_command),
/// the only supported way to change a subscription. The tables a durable store
/// keeps (`lash_*` in the first-party SQL backends) are private to lash; raw SQL
/// against them is unsupported for reads and writes alike.
pub mod triggers {
    /// Trigger catalog state exposed to protocol and engine integrators.
    pub use lash_core::TriggerEventCatalog;
    /// Process-free [`TriggerStore`] for tests and single-process hosts, matching
    /// the in-memory backends [`persistence`](crate::persistence) and
    /// [`observe`](crate::observe) offer for their own store contracts.
    pub use lash_core::facade_support::InMemoryTriggerStore;
    pub use lash_core::facade_support::deterministic_subscription_id;
    pub use lash_core::{
        LashSchema, TriggerCommandOutcome, TriggerDeliveryReservation,
        TriggerDeliveryReservationOutcome, TriggerDeliveryRetentionCandidate, TriggerEffectResult,
        TriggerIngressReceipt, TriggerInputBinding, TriggerMutationOutcome, TriggerMutationReceipt,
        TriggerOccurrenceFilter, TriggerOccurrenceReclamationReport,
        TriggerOccurrenceReclamationResult, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
        TriggerOperationError, TriggerOwnerScope, TriggerRetentionReconciliationReport,
        TriggerSubscriptionDraft, TriggerSubscriptionFilter, TriggerSubscriptionRecord,
        facade_support::TriggerDeliveryEmitOutcome, facade_support::TriggerDeliveryEmitReceipt,
        facade_support::TriggerEmitReport, facade_support::TriggerEvent,
        facade_support::TriggerEventType, facade_support::TriggerRegistration,
        facade_support::TriggerTarget, facade_support::empty_trigger_source_key,
    };
    /// The fenced, receipted verb vocabulary for subscription mutation,
    /// including [`TriggerCommand::Enable`] for re-enable, executed by
    /// [`TriggerStore::execute_command`] on the host's trigger store.
    pub use lash_core::{TriggerCommand, TriggerStore};
}

/// Tool definitions, providers, and execution types.
pub mod tools {
    pub use crate::tool_intent_ingress::{
        ToolIntentIngress, ToolIntentIngressKey, ToolIntentIngressOutcome, ToolIntentIngressRefusal,
    };
    /// Typed cancellation evidence constructed by tool implementors; pass it to
    /// [`ToolCallOutput::cancelled`] when a tool stops without completing.
    pub use lash_core::ToolCancellation;
    /// Turn flow control constructed by tool implementors; attach it with
    /// [`ToolCallOutput::with_control`] or [`ToolOutcome::with_control`].
    pub use lash_core::ToolControl;
    /// Per-tool retry policy carried by [`ToolDefinition::with_retry_policy`].
    pub use lash_core::ToolRetryPolicy;
    /// Collected replies returned by a runtime tool batch.
    pub use lash_core::session::ToolBatchReplies;
    /// Trigger-effect outcome returned through the tool dispatch context.
    pub use lash_core::tool_dispatch::ToolTriggerEffectOutcome;
    pub use lash_core::{
        AttemptContext, AttemptProcessReads, AttemptSessionReads, CancelHint, CancelProcessIntent,
        CompactToolContract, EmitProcessEventIntent, EmitTriggerIntent, PendingAnnouncement,
        PendingCompletion, PreparedToolCall, ProcessParentEndPolicy, SignalProcessIntent,
        StartProcessIntent, TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT,
        TOOL_INTENT_MAX_PER_KIND, TOOL_INTENT_PROTOCOL_V1, TimeoutBehavior, ToolActivation,
        ToolArgumentProjectionPolicy, ToolAttachmentClient, ToolAttemptOutcome, ToolCall,
        ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolCatalogEntry, ToolContext,
        ToolContract, ToolDefinition, ToolDirectCompletionClient, ToolDispatchClient,
        ToolExecutionGrant, ToolFailure, ToolFailureClass, ToolFailureSource, ToolIntent,
        ToolIntentExecutionOutcome, ToolIntentIdentity, ToolIntentKind, ToolIntentParentEnd,
        ToolIntentParentEndAction, ToolIntentParentEndOutcome, ToolIntentRefusalReason,
        ToolIntents, ToolManifest, ToolOutcome, ToolOutcomeDone, ToolOutputContract,
        ToolPrepareCall, ToolPrepareContext, ToolProcessEventClient, ToolProvider, ToolRegistry,
        ToolRetryStatus, ToolSessionAdmin, ToolSessionModel, ToolValue,
        derive_tool_intent_identity, facade_support::OrchestrationContext,
        facade_support::ReconfigureError, facade_support::ToolRegistryFacadeOps,
        facade_support::ToolSourceHandle, facade_support::ToolStateFacadeOps,
        facade_support::ToolTriggerClient, turn_outcome_from_tool_control,
    };
    pub use lash_core::{InternalProcessAdmin, InternalProcessContext, InternalProcessToolCall};
    /// Tool-execution request batches, replies, and child-process observation hooks.
    pub use lash_core::{
        PreparedToolBatch, PreparedToolBatchCall, ToolChildExecutionTraceHook,
        ToolChildProcessStarted, facade_support::OrchestratingToolDef,
        facade_support::ToolInvocation, facade_support::ToolInvocationReply,
    };
    pub use lash_core::{
        ToolId, ToolState, facade_support::PLUGIN_TOOL_SOURCE_ID,
        facade_support::ToolRestoreReport, facade_support::ToolStateEntry,
    };
    /// Runtime-owned tool-intent admission records used by process-registry integrators.
    pub use lash_core::{ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord};
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, LASHLANG_TOOL_BINDING_KEY,
        RemoteToolGrantBindingExt, ToolBinding, ToolDefinitionBindingExt, ToolManifestBindingExt,
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

/// Direct protocol transport types.
pub mod direct {
    pub use lash_core::llm::types::{
        AttachmentSource, GenerationOptionOutcome, GenerationOptions, GenerationReceipt,
        LlmEventSender, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
        NonNegativeFiniteF64, NonNegativeFiniteF64Error, ProviderFileScope,
        ProviderReasoningReplay, ProviderReplayDrop, ProviderReplayDropReason, ProviderReplayKind,
        ProviderRouteIdentity,
    };
    pub use lash_core::{
        facade_support::DirectCompletion, facade_support::DirectJsonSchema,
        facade_support::DirectLlmClient, facade_support::DirectLlmCompletion,
        facade_support::DirectLlmError, facade_support::DirectLlmOutcome,
        facade_support::DirectMessage, facade_support::DirectOutputSpec,
        facade_support::DirectPart, facade_support::DirectRequest, facade_support::DirectRole,
    };
}

/// Session persistence types and services.
pub mod persistence {
    /// Diagnostic read over a session's execution lease: holder identity,
    /// generation, expiry, and renewal state. Snapshot only: the commit CAS is
    /// the authority (ADR 0029). Entry point:
    /// [`LashCore::session_lease_diagnostics`](crate::LashCore::session_lease_diagnostics).
    pub use crate::session_lease::{
        SessionLeaseDiagnostics, SessionLeaseHolder, SessionLeaseRenewal,
    };
    pub use lash_core::CheckpointKind;
    pub use lash_core::facade_support::FileAttachmentStore;
    /// Durable session-store inputs and outputs exposed to storage integrators.
    pub use lash_core::runtime::{
        DeliveryPolicy, ForkPoint, ForkSessionReceipt, ForkSessionRequest, InMemorySessionStore,
        InMemorySessionStoreFactory, LiveReplayOutcome, LiveReplaySubscription,
        PROCESS_WAKE_MERGE_KEY, PendingTurnInputClaimDiagnostics, PendingTurnInputDraft,
        ProcessWakeSource, QueuedCheckpointTurnInput, QueuedCheckpointWork, QueuedTurnWork,
        QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
        QueuedWorkClaimBoundary, QueuedWorkClaimData, QueuedWorkClaimPolicy, QueuedWorkCompletion,
        QueuedWorkCompletionData, QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind,
        QueuedWorkPayload, RuntimeCheckpointComponents, RuntimeSessionState, SessionCursorError,
        SessionStoreCreateRequest, SessionStoreFactory, TurnInputCheckpointBoundary,
        TurnInputClaim, TurnInputClaimData, TurnInputClaimMode, TurnInputCompletion,
        TurnInputCompletionData, TurnInputIngress, TurnInputSettlementClaim, TurnInputState,
        UnclaimedTurnInputs,
    };
    pub use lash_core::session_graph::RealizedNodeTimestamp;
    /// Store-integrator attachment-manifest contracts and their persisted vocabulary.
    pub use lash_core::{
        AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwnerKind,
    };
    pub use lash_core::{QueuedWorkClaimOutcome, SelectedQueuedWorkClaimOutcome};
    /// Queued-work state, leases, and execution types.
    pub mod queued_work {
        /// Stable queued-work ordering values and selection helpers for store implementations.
        pub use lash_core::store::queued_work::{
            PendingSessionWorkOrdering, PendingWorkOrderingKey, QueuedWorkClass, claim_scan_limit,
            derive_batch_id, select_exact_turn_work_claim_prefix, select_leading_session_command,
            select_turn_work_claim_prefix,
        };
    }
    pub use lash_core::store::{
        AppendRequestIdentity, CheckpointComponentDescriptor, GraphAppend,
        HydratedCheckpointComponent, HydratedSessionCheckpoint, OperationId,
        OrphanedTurnInputScope, PersistedSessionRead, RuntimeCommit, RuntimeCommitReceipt,
        RuntimeTurnCommitStamp, RuntimeUsageDelta, RuntimeUsageDeltaIdentity, SessionCheckpoint,
        SessionHead, SessionHeadMeta, SessionHeadPayload, commit_runtime_state_verified,
        load_persisted_session_state,
    };
    pub use lash_core::{
        AttachmentCondemnation, AttachmentDeleteArming, AttachmentReclamationPolicy,
        AttachmentRootSet, AttachmentStore, AttachmentStoreError, AttachmentStorePersistence,
        AttachmentWriteFence, EmptyRootSetPolicy, ProcessExecutionEnvStore, StoredAttachment,
        StoredBlobRef, attachments::AttachmentReclamationFailure,
        facade_support::AttachmentGcFence, facade_support::AttachmentReclamationReport,
        facade_support::InMemoryAttachmentStore, facade_support::InMemoryProcessExecutionEnvStore,
        facade_support::SessionAttachmentStore, facade_support::reclaim_unreferenced_attachments,
    };
    pub use lash_core::{
        BlobRef, CURRENT_SESSION_STATE_VERSION, DurableItem, DurablePayload, DurableScan,
        DurableScanPage, DurableSurface, ExecutedCallOutcome, ExecutedCallRecord, GcReport,
        LeaseClaimNonce, LeaseOwnerIdentity, MaintenanceFailure, MaintenanceRefusal,
        MaintenanceReport, MaintenanceResult, MaintenanceStop, MaintenanceSweep,
        OLDEST_SUPPORTED_SESSION_STATE_VERSION, PersistedSessionConfig, PersistedTurnState,
        ProtocolEvent, QueuedWorkStore, RuntimePersistence, ScanCoverage, SessionAdmission,
        SessionBinding, SessionBlobReclaimReport, SessionCommitStore, SessionExecutionLease,
        SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
        SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseDisplacement,
        SessionExecutionLeaseRenewalInstallMismatch, SessionExecutionLeaseStore, SessionGraph,
        SessionHistoryRecord, SessionMeta, SessionNodePayload, SessionNodeRecord, SessionReadView,
        SessionRelation, SessionStateAdmission, StoreBackend, StoreError, StoreMaintenance,
        StorePreflight, StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus,
        StoreSchemaVerdict, TurnId, TurnInputStore, VacuumReport, WorkClaim, WorkCompletion,
        facade_support::SessionNodeProjection,
    };
    /// Committed session history flattened into presentation order, as returned
    /// by [`SessionReadView::chronological_projection`].
    pub use lash_core::{
        facade_support::ChronologicalEntry, facade_support::ChronologicalPayload,
        facade_support::ChronologicalProjection,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{InMemoryLashlangArtifactStore, LashlangArtifactStore};
}

/// Plugin contracts, manifests, and operation types.
pub mod plugins {
    pub use lash_core::PluginOptions;
    /// Host-specialized driver configuration required by every [`TurnDriverPreamble`].
    pub use lash_core::TurnDriverConfig;
    /// Durable session-lifecycle operations a hook context carries, alongside
    /// [`SessionStateService`] and [`SessionGraphService`]. Named by
    /// [`TurnTransformContext`] and [`CompactionContext`]; runtime-implemented.
    pub use lash_core::facade_support::SessionLifecycleService;
    pub use lash_core::facade_support::{
        AbortTurnDirective, AfterToolCallPluginDirective, AfterTurnPluginDirective,
        BeforeToolCallPluginDirective, EnqueueMessagesDirective, PluginDirective,
        ReplaceToolArgsDirective, ShortCircuitToolDirective, TurnPluginDirective,
    };
    /// Hook contracts and reports used by plugin authors.
    pub use lash_core::plugin::{
        AfterToolCallHook, AfterTurnHook, AssistantResponseHook, AssistantResponseHookContext,
        AssistantResponseTransform, AssistantStreamFinishReason, AssistantStreamFinishedContext,
        AssistantStreamHook, AssistantStreamHookContext, AssistantStreamTransform,
        BeforeToolCallHook, BeforeTurnHook, CheckpointHook, CheckpointHookContext,
        CompactionContext, ContextCompaction, ContextCompactor, ContextError,
        PluginExtensionContribution, PluginSpecBuilder, StaticPluginFactory, ToolCallHookContext,
        ToolCatalogContext, ToolResultHookContext, ToolResultProjectionContext, TurnHookReport,
    };
    /// Protocol and process-engine contracts, including their complete runtime-owned state closure.
    pub use lash_core::plugin::{
        CheckpointApplication, CodeExecutorPlugin, ExecutionStateComponentSnapshot,
        ExecutionStateSnapshot, HydratedExecutionState, PluginAbort, PluginSessionSnapshot,
        PluginSnapshotArtifact, PluginSnapshotEntry, PrepareTurnRequest,
        ProtocolBeforeLlmCallContext, ProtocolDriverPlugin, ProtocolLlmCallAction,
        ProtocolRuntimeContext, ProtocolSessionContext, ProtocolSessionMaterialization,
        ProtocolSessionPlugin, SessionAuthorityContext, TurnFinalization, TurnPreparation,
    };
    /// Plugin operations: the query / command / task vocabulary. A plugin
    /// author declares an operation by implementing [`PluginOperation`] plus
    /// one of [`PluginQuery`], [`PluginCommand`] or [`PluginTask`], registers a
    /// handler through
    /// [`PluginRegistrar::operations`](lash_core::plugin::PluginRegistrar::operations)
    /// or [`PluginSpec`], and receives the matching context
    /// ([`PluginQueryContext`], [`PluginCommandContext`], [`PluginTaskContext`]).
    /// Command and task handlers return a [`PluginOperationOutcome`], which is
    /// how a plugin asks the runtime to do something on its behalf — today one
    /// [`PluginRuntimeDirective`]. Hosts invoke operations through
    /// [`PluginOperations`](crate::admin::PluginOperations) and read the
    /// resulting [`PluginOperationReceipt`].
    ///
    /// This is authoring surface in full: writing a plugin that carries
    /// operations needs no `lash-core` dependency (ADR 0051).
    pub use lash_core::plugin::{
        PluginCommand, PluginCommandContext, PluginOperation, PluginOperationDef,
        PluginOperationFailure, PluginOperationInvokeError, PluginOperationKind,
        PluginOperationOutcome, PluginOperationReceipt, PluginOwned, PluginQuery,
        PluginQueryContext, PluginRuntimeDirective, PluginTask, PluginTaskContext,
        ProcessReadService, SessionParam, SessionReadService,
    };
    /// Durable plugin state: what [`SessionPlugin::snapshot`] writes and
    /// [`SessionPlugin::restore`] reads back, plus the readiness context a
    /// plugin is handed once its session exists. The aggregate the runtime
    /// persists (`PluginSessionSnapshot` and its entries) is an integrator
    /// seam and stays on `lash-core`: a plugin writes blobs and returns its own
    /// [`PluginSnapshotMeta`], and never names the collection they land in.
    pub use lash_core::plugin::{
        PluginSnapshotMeta, SessionReadyContext, SnapshotReader, SnapshotWriter,
    };
    /// Engine registry and narrowed execution contexts used to host custom process engines.
    pub use lash_core::runtime::{
        ProcessEngineProcessContext, ProcessEngineRegistry, ProcessEngineRunGuard,
        ProcessEngineRuntimeContext,
    };
    /// Protocol-driver and process-engine inputs that core owns independently of plugin storage.
    pub use lash_core::{
        AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord, HostTurnProtocol,
        PersistedSegmentHandover, ProcessEngine, ProcessEngineRunContext,
        ProcessEngineValidationContext, ProcessInfraError, ProcessRunOutcome, ProtocolBuildInput,
        ProtocolDriverState, ProtocolTurnExtension, ProtocolTurnOptionsError, SegmentHandover,
        SessionContextOverlay, SessionPluginSource, TurnDriverPreamble,
    };
    /// The session services a hook context hands a plugin: read-through state
    /// access ([`SessionStateService`]) and durable graph appends
    /// ([`SessionGraphService`]), plus the append request/result vocabulary.
    /// Both are runtime-implemented — a plugin receives one, never writes one.
    pub use lash_core::{
        AppendSessionNodesOutcome, AppendSessionNodesRequest, PluginExtensions, SessionAppendNode,
        SessionGraphService, SessionStateService, SessionToolAccess, SubagentSessionContext,
    };
    /// Code-executor request, response, and runtime capability context.
    pub use lash_core::{ExecRequest, ExecResponse, RuntimeExecutionContext};
    pub use lash_core::{
        PluginError, PluginMessage, PluginRuntimeEvent, ToolCatalog, facade_support::PluginFactory,
        facade_support::PluginHost, facade_support::PluginRegistrar, facade_support::PluginSession,
        facade_support::PluginSessionContext, facade_support::PluginSpec,
        facade_support::PluginSpecFactory, facade_support::PromptHookContext,
        facade_support::SessionPlugin, facade_support::ToolCatalogContribution,
        facade_support::TurnHookContext, facade_support::TurnResultHookContext,
    };
    /// Lifecycle observation: what a `reg.session().on_event(..)` hook receives
    /// once durable session state has advanced, and the contexts each event
    /// carries. [`PluginLifecycleEvent::TurnPersisted`] fires after the commit it
    /// describes, so a hook observes a session whose head may already have moved
    /// on.
    pub use lash_core::{
        facade_support::PluginLifecycleEvent, facade_support::SessionConfigChangedContext,
        facade_support::SessionStateChangedContext,
    };
    /// Per-turn context assembly: the prepared messages, prompt contributions,
    /// and tool providers a [`TurnContextTransform`] may rewrite before the
    /// model call, and the read-only context the transform is handed.
    pub use lash_core::{
        facade_support::PreparedContext, facade_support::TurnContextTransform,
        facade_support::TurnTransformContext,
    };
    pub use lash_plugin_tool_output_budget::{
        ToolOutputBudgetConfig, ToolOutputBudgetMode, ToolOutputBudgetPluginFactory,
        tool_output_budget_stack as runtime_plugin_stack,
    };
    /// Default chat projector installed by [`TurnDriverConfig::chat`].
    pub use lash_sansio::ChatContextProjector;
    /// Projection contract stored by [`TurnDriverConfig`] when a protocol supplies a custom
    /// context projector.
    pub use lash_sansio::ContextProjector;
    /// In-process prompt identity carried by [`TurnDriverPreamble::tool_names_fingerprint`].
    pub use lash_sansio::PromptFingerprint;
    /// Sans-I/O protocol handle accepted by [`TurnDriverConfig::chat`]; custom host drivers use
    /// [`HostTurnProtocol`] as its protocol parameter.
    pub use lash_sansio::ProtocolDriverHandle;
    /// Callback used by [`TurnDriverConfig`] to materialize the terminal turn-limit message.
    pub use lash_sansio::TurnLimitFinalMessage;
    /// Model-facing tool declaration carried by [`TurnDriverPreamble::tool_specs`].
    pub use lash_sansio::llm::types::LlmToolSpec;
}

/// Protocol message and content types.
pub mod messages {
    /// Read-side session-tree values exposed through integrator session views.
    pub use lash_core::session_graph::{SessionMessageTreeNode, SharedJsonValue};
    pub use lash_core::{
        Message, MessageOrigin, MessageRole, Part, PartKind, facade_support::MessageSequence,
        session_model::message::PartAttachment,
    };
}

/// Attachment values: identity, media type, and the metadata that travels with
/// bytes. This is the vocabulary shared by the three places a host meets an
/// attachment — [`InputItem::attachment`](crate::InputItem), the direct-LLM
/// [`AttachmentSource`](crate::direct::AttachmentSource), and the
/// [`AttachmentStore`](crate::persistence::AttachmentStore) contract — so it
/// has its own home rather than being duplicated into each.
///
/// Where the bytes live is a persistence concern:
/// [`persistence`] carries the store trait, its errors, and reclamation.
pub mod attachments {
    pub use lash_core::{
        AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, MediaType,
        facade_support::AttachmentMeta,
    };
    pub use lash_sansio::{InvalidAttachmentId, InvalidMediaType};
}

/// Wire-format DTOs for driving lash across a process boundary, sub-namespaced
/// by protocol domain. Only the cross-cutting envelope
/// ([`Envelope`](remote::Envelope),
/// [`REMOTE_PROTOCOL_VERSION`](remote::REMOTE_PROTOCOL_VERSION)) and the
/// protocol error type live at this root; everything else has exactly one
/// home in a domain sub-namespace.
pub mod remote {
    pub use lash_remote_protocol::{Envelope, REMOTE_PROTOCOL_VERSION, RemoteProtocolError};

    /// LLM request/response envelopes: messages, attachments, tool specs,
    /// output specs, and provider metadata.
    pub mod llm {
        pub use lash_remote_protocol::llm::{
            RemoteAttachmentRef, RemoteAttachmentSource, RemoteAttachmentTypeMetadata,
            RemoteAttemptOutcome, RemoteAttemptRecord, RemoteDiagnostic, RemoteExecutionEvidence,
            RemoteExecutionEvidenceCollectionInterruption, RemoteGenerationOptionOutcome,
            RemoteGenerationOptions, RemoteGenerationReceipt, RemoteLlmCallRecord,
            RemoteLlmContentBlock, RemoteLlmMessage, RemoteLlmOutputPart, RemoteLlmOutputSpec,
            RemoteLlmRequest, RemoteLlmRequestScope, RemoteLlmResponse, RemoteLlmRole,
            RemoteLlmTerminalReason, RemoteLlmToolChoice, RemoteLlmToolSpec, RemoteModelCapability,
            RemoteModelIntent, RemoteNormalizedError, RemoteProtocolPosition,
            RemoteProviderFailureKind, RemoteProviderFileScope, RemoteProviderMetadata,
            RemoteProviderReasoningReplay, RemoteProviderReplayDrop,
            RemoteProviderReplayDropReason, RemoteProviderReplayKind, RemoteProviderReplayMeta,
            RemoteProviderRouteIdentity, RemoteReasoningCapability, RemoteReasoningDisableEncoding,
            RemoteReasoningEncoding, RemoteReasoningSelection, RemoteResponseTextMeta,
            RemoteRetryDecision, RemoteSchemaProjectionOverride,
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
            RemoteObservedProcess, RemoteObservedProcessEvent, RemotePersistProcessEnvReceipt,
            RemotePersistProcessEnvRequest, RemoteProcessAwaitOutcome, RemoteProcessAwaitOutput,
            RemoteProcessAwaitRequest, RemoteProcessCancelReceipt, RemoteProcessCancelRequest,
            RemoteProcessDefinitionIdentity, RemoteProcessEvent, RemoteProcessEventSemantics,
            RemoteProcessEventSemanticsSpec, RemoteProcessEventType, RemoteProcessEventsRequest,
            RemoteProcessEventsResponse, RemoteProcessExecutionEnvRef,
            RemoteProcessExecutionEnvSpec, RemoteProcessExecutionPolicy, RemoteProcessExternalRef,
            RemoteProcessHandleView, RemoteProcessInput, RemoteProcessListFilter,
            RemoteProcessListResponse, RemoteProcessModelLimits, RemoteProcessModelSpec,
            RemoteProcessOriginator, RemoteProcessPluginOptions, RemoteProcessProvenance,
            RemoteProcessSignalReceipt, RemoteProcessSignalRequest, RemoteProcessStartReceipt,
            RemoteProcessStartRequest, RemoteProcessStarted, RemoteProcessStatus,
            RemoteProcessStatusFilter, RemoteProcessTerminalSemantics, RemoteProcessTerminalSpec,
            RemoteProcessValueSelector, RemoteProcessWaitKind, RemoteProcessWaitState,
            RemoteProcessWake, RemoteProcessWakeSpec, RemoteProcessWorkItem,
            RemoteProcessWorkSnapshot, RemoteRecoveryContract, RemoteRuntimeEffectKind,
            RemoteRuntimeInvocation, RemoteRuntimeReplay, RemoteRuntimeScope, RemoteRuntimeSubject,
            RemoteSessionScope, RemoteToolFailureClass, RemoteTurnBudget,
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
            RemoteTriggerDeliveryEmitOutcome, RemoteTriggerDeliveryEmitReceipt,
            RemoteTriggerEmitReport, RemoteTriggerInputBinding, RemoteTriggerInputTemplate,
            RemoteTriggerListSubscriptionsResponse, RemoteTriggerOccurrenceRecord,
            RemoteTriggerOccurrenceRequest, RemoteTriggerRegisterSubscriptionReceipt,
            RemoteTriggerRegisterSubscriptionRequest, RemoteTriggerRegistration,
            RemoteTriggerSubscriptionDraft, RemoteTriggerSubscriptionFilter,
            RemoteTriggerSubscriptionRecord, RemoteTriggerTarget,
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
            RemoteTurnCancelDisposition, RemoteTurnCancelOutcome, RemoteTurnCancelReceipt,
            RemoteTurnCancelRequest, RemoteTurnCancellationEvidence,
        };
    }

    /// Turn result envelopes: outcomes, stops, assistant output, summaries,
    /// issues, and causal references.
    pub mod turn_result {
        pub use lash_remote_protocol::turn_result::{
            RemoteAssistantOutput, RemoteAssistantOutputState, RemoteCausalRef,
            RemoteToolCallOutcome, RemoteToolCallRecord, RemoteTurnExecutionMetrics,
            RemoteTurnFinish, RemoteTurnIssue, RemoteTurnOutcome, RemoteTurnReport,
            RemoteTurnStatus, RemoteTurnStop, RemoteTurnUsageReport,
        };
    }

    /// Token usage accounting and the streaming turn-activity vocabulary.
    pub mod usage {
        pub use lash_remote_protocol::usage_activity::{
            RemoteTokenLedgerEntry, RemoteTurnActivity, RemoteTurnEvent, RemoteUsage,
        };
    }
}

/// Durable process definitions, handles, and events.
pub mod process {
    pub use crate::admin::SessionProcessAdmin;
    pub use crate::process_admin::Processes;
    /// Materialized event semantics returned to custom process registries.
    pub use lash_core::runtime::ProcessEventSemantics;
    /// Process-registry and event types that complete the store and engine signature closure.
    pub use lash_core::runtime::{
        ObserverInheritance, ProcessChange, ProcessCompletionOutcome,
        ProcessExecutionConcurrencyError, ProcessExecutionWriteAuthority, ProcessId,
        ProcessOutcome, ProcessParentEndPlan, ProcessStartOutcome, ProcessTerminalSemantics,
        ProcessTerminalSpec, ProcessTombstone, SessionId, WaitKind, WaitState, WakeDelivery,
        WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome, WakeDeliveryDisposition,
        WakeDeliveryReport, WakeDeliveryState, WakeDiscardReason,
    };
    pub use lash_core::{
        AbandonEvidence, AbandonRequest, AbandonWriter, CausalRef, ProcessAwaitOutput,
        ProcessCancelReceipt, ProcessChangeCursor, ProcessCompletionAuthority,
        ProcessContinuationStore, ProcessEvent, ProcessEventAppendReceipt,
        ProcessEventAppendRequest, ProcessEventType, ProcessExecutionContext,
        ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExternalRef, ProcessHandleView,
        ProcessIdentity, ProcessInput, ProcessLease, ProcessLeaseClaimOutcome,
        ProcessLeaseCompletion, ProcessListFilter, ProcessListMode, ProcessLiveReferenceView,
        ProcessObserverBy, ProcessOpScope, ProcessOriginator, ProcessProvenance,
        ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessService,
        ProcessSessionDeleteReport, ProcessStartOptions, ProcessStartRequest, ProcessStarted,
        ProcessStatus, ProcessStatusFilter, ProcessWakeDelivery, ProcessWakeSpec,
        ProcessWorklistCursor, ProcessWorklistPage, ProjectionWatermark, RecoveryContract,
        SessionScope, facade_support::ObservedProcess, facade_support::ObservedProcessEvent,
        facade_support::ObservedWorkItem, facade_support::ProcessAdmissionDeferred,
        facade_support::ProcessAdmissionIntake, facade_support::ProcessAdmissionReport,
        facade_support::ProcessAttach, facade_support::ProcessAwaiter,
        facade_support::ProcessChangeHub, facade_support::ProcessEventSink,
        facade_support::ProcessRunHandle, facade_support::ProcessRuntimeHost,
        facade_support::ProcessToolVisibilityFilter, facade_support::ProcessWake,
        facade_support::ProcessWorkDriver, facade_support::ProcessWorkObserver,
        facade_support::ProcessWorkSnapshot, facade_support::ProcessWorkerFault,
        facade_support::SessionScopeId, facade_support::watch_process_registry,
        facade_support::watch_process_registry_with_sink,
    };
    /// Event semantics a registration declares for its extra event types: which
    /// occurrences wake the process ([`ProcessWakeSpec`]) and how a payload is
    /// projected into the wake input ([`ProcessValueSelector`]).
    pub use lash_core::{ProcessEventSemanticsSpec, ProcessValueSelector};
    /// Wake redelivery. A host that owns its own [`ProcessRegistry`] also owns
    /// the redelivery loop that turns pending wakes into queued work; an
    /// embedded core drives one for you.
    /// [`process_wake_source_key`] is the queued-work source key a delivered
    /// wake lands under, so a host can correlate the two.
    pub use lash_core::{
        WakeDeliveryConfig, facade_support::WakeDeliveryDriveReport,
        facade_support::WakeDeliveryDriver, facade_support::process_wake_source_key,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        LASHLANG_ENGINE_KIND, LashlangProcessInput, lashlang_process_event_types,
        lashlang_process_signal_event_types,
    };
}

/// Durability configuration and backend contracts.
pub mod durability {
    /// Reject a [`TurnInput`](crate::TurnInput) that a durable
    /// [`EffectHost`] cannot replay — live protocol extensions and live plugin
    /// inputs have no journalled form. The embedded enqueue path applies this
    /// itself; a host that accepts turn input at its own edge calls it there to
    /// fail the request instead of the turn.
    pub use lash_core::facade_support::ensure_durable_effect_input;
    pub use lash_core::facade_support::{
        ProcessDrainDeferred, ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation,
    };
    /// Effect-host inputs, replay projections, and local execution capabilities.
    pub use lash_core::runtime::{
        BoundaryReason, CanonicalRuntimeEffectEnvelope, EffectJournalIdentity,
        EffectJournalRetirement, ProcessLocalExecution, ProcessOutcomeObserver,
        ProcessTurnCancellation, RuntimeAwaitEventOptions, RuntimeEffectReplayTrace, RuntimeReplay,
        RuntimeReplayAttribution, RuntimeSleepOptions, RuntimeSubject, SegmentProgress,
        ToolAttemptLaunch, ToolCallLaunch, TriggerLocalExecution,
    };
    pub use lash_core::{
        EffectHost, EffectJournalAddressing, facade_support::DurableProcessWorker,
        facade_support::DurableProcessWorkerConfig, facade_support::InlineEffectHost,
        facade_support::LeaseTimings, facade_support::LeaseTimingsError,
        facade_support::ProcessDrainReport, facade_support::RuntimeEnvironment,
        facade_support::RuntimeHostConfig, facade_support::TerminationPolicy,
    };
}

/// Runtime events, errors, and execution controls.
pub mod runtime {
    pub use crate::core::AdvancedLashCoreBuilder;
    /// Prompt-token accounting a [`TurnContextTransform`](crate::plugins::TurnContextTransform)
    /// is handed so a rolling strategy can budget against the last render.
    pub use lash_core::PromptUsage;
    /// Structured cause carried by a [`RuntimeError`], so a host distinguishes
    /// an expected retirement (a deleted session) from a real fault.
    pub use lash_core::RuntimeErrorCause;
    /// Assistant-output state exposed by assembled runtime turns.
    pub use lash_core::facade_support::OutputState;
    /// Runtime host configuration, control, observation, and effect contracts.
    pub use lash_core::runtime::{
        ApplyConfigPatch, AssembledTurn, AssistantResponseHookEvents, AwaitEventResolver,
        CheckpointClaimSet, DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY, DirectCompletionClient,
        EffectGroupHandle, EffectGroupMembership, EmbeddedRuntimeHost, EventSink, ExecutionScope,
        GroupExecutors, GroupSettlement, GroupWakePolicy, InlineRuntimeEffectController,
        LashRuntime, LlmAttachmentSpec, LlmRequestSpec, LoserPolicy, NoopEventSink,
        NoopTurnActivitySink, ProcessCommand, ProcessEffectOutcome,
        QUEUED_WORK_SLOW_WAKE_THRESHOLD, QueuedWorkDriver, QueuedWorkExecutionConcurrencyError,
        QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunHandle, QueuedWorkRunProgress,
        QueuedWorkRunRequest, QueuedWorkSlowWake, QueuedWorkWakeContended, QueuedWorkWakeFailure,
        QueuedWorkWakeOutcome, RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeEffectCommand,
        RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
        RuntimeEffectGroup, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
        RuntimeEffectReplayMismatchReport, RuntimeEnvironmentBuilder, RuntimeError,
        RuntimeErrorCode, RuntimeHandle, RuntimeInvocation, RuntimeNamedPhase, RuntimeObservation,
        RuntimePromptConfig, RuntimeProviderConfig, RuntimeScope, RuntimeTracingConfig,
        RuntimeTurnPhase, RuntimeTurnPhaseProbe, RuntimeTurnPhaseProbeSlot, ScopedEffectController,
        TurnContext,
    };
    /// The host clock accepted by
    /// [`LashCoreBuilder::clock`](crate::LashCoreBuilder::clock), used for
    /// runtime sleeps and embedded
    /// store timestamps. [`SystemClock`] is the wall-clock default; tests supply
    /// their own to make expiry deterministic.
    pub use lash_core::{Clock, facade_support::SystemClock};
    /// Session and turn extension handles exposed to runtime integrators.
    pub use lash_core::{
        ProtocolSessionExtensionHandle, ProtocolTurnExtensionHandle, ProtocolTurnOptions,
        SessionPolicy, SessionSnapshot, facade_support::PersistentRuntimeServices,
        facade_support::SessionHandle, facade_support::render_turn_causes_prompt,
    };
}

/// Prompt templates, layers, and contributions.
pub mod prompt {
    pub use lash_core::{
        PromptBuiltin, PromptContribution, PromptContributionGate, PromptLayer, PromptSlot,
        PromptSlotLayer, PromptTemplate, PromptTemplateEntry, PromptTemplateSection,
        facade_support::default_prompt_template,
    };
}

/// Trace context, events, and sink configuration.
pub mod tracing {
    #[cfg(feature = "otel-trace")]
    pub use lash_core::{OtelTraceOptions, OtelTraceSink};
    pub use lash_core::{
        TraceAttachment, TraceContentBlock, TraceEffectEnvelopeDiffEntry,
        TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue, TraceError, TraceEvent,
        TraceLlmMessage, TraceLlmRequest, TraceLlmResponse, TracePromptComponent,
        TraceProviderReplayDropEvent, TraceProviderReplayDropReason, TraceProviderReplayKind,
        TraceProviderRequestEvent, TraceProviderRouteIdentity, TraceProviderStreamEvent,
        TraceRuntimeStreamEvent, TraceTokenUsage, TraceToolSpec, facade_support::JsonlTraceSink,
        facade_support::TraceBranchSelection, facade_support::TraceLabelMetadata,
        facade_support::TraceRecord, facade_support::TraceRuntimeScope,
        facade_support::TraceRuntimeSubject, facade_support::TraceSinkError,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
        TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
        TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLashlangEdgeSelection,
        TraceLashlangGraph, TraceLashlangGraphChildLink, TraceLashlangGraphEdge,
        TraceLashlangGraphNode, TraceLashlangGraphStore, TraceLashlangNodeObservation,
    };
    pub use lash_trace::{StderrTraceSink, TeeTraceSink, TraceContext, TraceLevel, TraceSink};
}

/// Test helpers for embedders. Enable with `lash = { ..., features = ["testing"] }`
/// to script model responses in integration tests without a live provider.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// Model-provider configuration and request types.
pub mod provider {
    /// Typed provider-failure classification surfaced on
    /// [`TurnIssue`](crate::turn::TurnIssue) and session error envelopes.
    pub use lash_core::ProviderFailureKind;
    /// Why a host-supplied [`ModelCapability`] rejected a reasoning-effort
    /// selection. The snake_case [`ModelEffortValidationCategory`] codes are a
    /// stable contract a capability catalog can branch on.
    pub use lash_core::facade_support::ModelEffortValidationCategory;
    pub use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmOutputSpec, LlmRole};
    pub use lash_core::provider::ModelEffortValidationError;
    /// Provider completion, caching, failure, retry, and rate-limiting contracts.
    pub use lash_core::provider::{
        CacheRetention, DefaultProviderFailureClassifier, ProviderCompletion,
        ProviderCompletionError, ProviderFailureClassifier, ProviderRateLimitPermit,
        ProviderRateLimitPolicy, ProviderRateLimiter, ProviderReliability, ProviderRetryPolicy,
        RequestTimeout,
    };
    /// Request/response/error vocabulary of [`Provider::complete`],
    /// re-exported so hosts can implement provider decorators (admission
    /// gates, metrics taps) against the facade alone.
    pub use lash_core::{
        AttemptOutcome, ExecutionEvidence, ExecutionEvidenceCollectionInterruption,
        ExecutionEvidenceMergeError, LlmRequest, LlmRequestScope, LlmResponse, LlmStreamEvidence,
        NormalizedError, ProtocolPosition, ProviderEndpointError,
        facade_support::LlmTransportError,
    };
    pub use lash_core::{
        CacheControlDialect, ModelCapability, ReasoningCapability, ReasoningDisableEncoding,
        ReasoningEncoding, ReasoningSelection, SamplingCapability, StreamTermination,
        facade_support::GenerationRetryGuarantee, facade_support::LlmTimeouts,
        facade_support::Provider, facade_support::ProviderComponents,
        facade_support::ProviderHandle, facade_support::ProviderOptions,
    };
}
