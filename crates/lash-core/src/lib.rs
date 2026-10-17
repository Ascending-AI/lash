//! Runtime kernel for Lash.
//!
//! Recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! The process kernel understands `SessionTurn` to coordinate child-session
//! turns. Executable process bodies, including work a host runs outside lash,
//! use `ProcessInput::Engine { kind, payload, launch }` and call ordinary recorded tool
//! attempts under their process's committed run records.
//!
//! Protocols follow the same boundary: core owns the `HostTurnProtocol` state
//! shape and the `ProtocolDriverPlugin` slot, while external protocol crates
//! provide the driver implementation.

#![deny(clippy::future_not_send)]

/// Re-exported so `impl_noop_attachment_referrers!` can paste an
/// `#[async_trait]` impl into crates that do not depend on `async-trait`
/// directly. Not part of the supported surface.
#[doc(hidden)]
pub use async_trait::async_trait;

pub use lash_core_execution::ActorContext;
pub use lash_core_execution::ExecutionOwner;
pub use lash_core_execution::IngressReservedSourceKeyRefusal;
pub use lash_core_execution::admitted_scope_wire;
pub use lash_core_execution::compat;
pub use lash_core_execution::direct;
pub(crate) use lash_core_execution::direct_completion_client;
pub use lash_core_execution::engine;
/// The durable store port (ruling #74), which the facade re-exports whole
/// from `lash::durable`.
#[doc(hidden)]
#[cfg(any(test, feature = "testing"))]
pub use lash_core_execution::process_id_for_test;
pub use lash_core_execution::process_id_from_handle_json;
/// What an execution's root owes when it commits.
pub use lash_core_execution::runtime::actor::claim_obligations::ExecutionDebts;
pub use lash_core_execution::runtime::actor::claim_obligations::{
    ObligationSite, ObligationStage, ObligationUse, RecordedWork, RequiresMismatch,
};
pub use lash_core_execution::runtime::actor::round::{
    CallOwner, ParkedCall, StagedPluginState, StoreLocalEffect, StoreLocalRows, parked,
};
pub use lash_core_execution::runtime::actor::round::{
    admitted_bindings_of, cell_report_of, tool_outcome_of,
};
/// Durable tool-effect format versions, re-exported for the format manifest.
pub use lash_core_execution::waits;

pub use lash_core_execution::ResolveAnswer;
pub use lash_core_ids::operational_metrics;
pub use lash_core_llm::llm;
pub(crate) use lash_core_llm::llm_profile;
pub use lash_core_store::attachments;
pub use lash_core_store::chronological;
pub use lash_core_store::config_transaction::{
    CORE_CONFIG_OWNER, ConfigCommandEntry, ConfigFault, ConfigRefusal, ConfigRefusalReason,
    ConfigResolution, ConfigResolutionDecision, ConfigTransactionOutcome, ConfigTransactionRecord,
    ConfigValueRole, CoreConfig, RecordedNamespaceCorrupt, RefusalSite,
};
pub use lash_core_store::impl_current_fleet_format;
pub use lash_core_store::impl_noop_attachment_referrers;
pub use lash_core_store::protocol_turn_options::{ProtocolTurnOptions, ProtocolTurnOptionsError};
pub use lash_core_store::surface_format;
pub use lash_durable as durable_port;
/// The relays of the obligation kinds a store set still arms.
pub use runtime::obligations;
/// A session's durable close, the point of no return of its deletion
/// (FIG-3600 S7).
pub use runtime::session_delete;
/// Re-exported so every effect implementation can spell
/// `await_next_settlement`'s cancellation parameter without taking a direct
/// `tokio-util` dependency of its own (FIG-2266).
pub use tokio_util::sync::CancellationToken;
/// Panic containment for runtime-owned work.
///
/// The module lives in `lash-core-ids`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helpers crate-internal.
pub mod panic_containment {
    pub(crate) use lash_core_ids::panic_containment::payload_message;
    pub use lash_core_ids::panic_containment::{is_loud, set_loud};
}

pub use lash_core_execution::plugin;
pub(crate) use lash_core_execution::plugin_stack;
pub(crate) use lash_core_execution::protocol_build;

pub use lash_core_execution::{hook_key, tool_provider_key};
#[cfg(feature = "perf-witness")]
pub use lash_core_ids::perf_witness;
/// Provider components for pluggable LLM backends.
///
/// The module lives in `lash-core-llm`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helper crate-internal.
pub mod provider {
    pub(crate) use lash_core_llm::core_internal::{
        ModelCallBounds, call_id_for_scope, complete_prepared, prepare_completion,
        synthetic_terminal_call_record,
    };
    pub use lash_core_llm::provider::*;
}
pub mod runtime;
pub use lash_core_execution::session;
pub use lash_core_execution::session_model;
pub use lash_core_store::prompt_sections;
pub use lash_core_store::session_graph;
/// Stable hashing primitives, re-exported from `lash-core-ids`. The helpers
/// stay crate-internal; the module itself is public under `testing` exactly as
/// it was before the carve-out.
#[cfg(feature = "testing")]
pub mod stable_hash {
    pub use lash_core_ids::stable_hash::sha256_hex;
}
pub use lash_core_execution::store;
pub use lash_core_ids::task;
pub use lash_core_store::store_backend_support;
/// The tool-run contract's pinned seams (FIG-4867).
pub use lash_core_store::tool_run;
/// Standard-lock poison recovery traits used across Lash hosts and runtimes.
pub mod sync {
    pub use lash_protocol_types::sync::*;
}
#[cfg(any(test, feature = "testing"))]
pub use lash_core_execution::test_support;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_ids::test_watchdog;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use lash_core_execution::tool_dispatch;
#[cfg(feature = "testing")]
pub use lash_core_execution::tool_provider;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::tool_provider;
pub use lash_core_execution::tool_registry;
pub(crate) use lash_core_execution::tool_result;
pub use lash_core_execution::trace;

pub mod facade_support {
    pub use crate::runtime::effect::scope_status;
    pub use crate::runtime::{DurableSessionOps, EMPTY_HEAD_REVISION, QueueWithdrawalObservation};

    pub use lash_core_execution::runtime::process::steps::{
        RecordedProcessStep, process_step_call, read_process_steps, read_process_tool_call,
    };
    /// The shift-tracing seam a durable substrate implements against (the trace runtime,
    /// a step's issue and its standing), public in every feature variant.
    pub use lash_core_execution::trace::{
        JournalFrontier, LiveStep, OpenSpan, StepIssue, TraceBoundaryReceipt, TraceRuntime,
        TraceStanding,
    };
    pub use lash_core_ids::operational_metrics::StoreObserver;
    pub use lash_core_llm::core_internal::ProviderCompletionSideband;
    #[doc(hidden)]
    pub use lash_protocol_types::core_support::TOOL_BINDING_KEY;
    /// Apply the canonical runtime invocation projection to an existing trace
    /// context. Durable hosts use this instead of maintaining a second
    /// projection with different parent or attribution precedence.
    pub fn trace_context_for_runtime_invocation(
        context: lash_trace::TraceContext,
        invocation: &crate::RuntimeInvocation,
    ) -> lash_trace::TraceContext {
        crate::trace::trace_context_for_invocation(context, invocation)
    }

    /// Apply the canonical effect-header projection to an existing trace context.
    pub fn trace_context_for_runtime_effect_invocation(
        context: lash_trace::TraceContext,
        invocation: &crate::RuntimeEffectInvocation,
    ) -> lash_trace::TraceContext {
        crate::trace::trace_context_for_effect_invocation(context, invocation)
    }
    pub use crate::runtime::run_head_advancing_commit_attempt;

    pub use crate::attachments::AttachmentGcFence;
    pub use crate::attachments::AttachmentPolicy;
    pub use crate::attachments::AttachmentReclamationPolicy;
    pub use crate::attachments::AttachmentReclamationReport;
    pub use crate::attachments::EmptyRootSetPolicy;
    pub use crate::attachments::RuntimeAttachmentStore;
    pub use crate::attachments::reclaim_unreferenced_attachments;
    pub use crate::chronological::BorrowedChronologicalEntry;
    pub use crate::chronological::BorrowedChronologicalMessage;
    pub use crate::chronological::BorrowedChronologicalPayload;
    pub use crate::chronological::ChronologicalEntry;
    pub use crate::chronological::ChronologicalPayload;
    pub use crate::chronological::ChronologicalProjection;
    pub use crate::chronological::visit_turn_view;
    pub use crate::direct::DirectJsonSchema;
    pub use crate::direct::DirectLlmClient;
    pub use crate::direct::DirectLlmError;
    pub use crate::direct::DirectLlmOutcome;
    pub use crate::direct::DirectMessage;
    pub use crate::direct::DirectOutputSpec;
    pub use crate::direct::DirectPart;
    pub use crate::direct::DirectRequest;
    pub use crate::direct::DirectRole;

    pub use crate::plugin::AfterTurnContributions;
    pub use crate::plugin::AssistantResponseTransform;
    pub use crate::plugin::CheckpointHookContext;
    pub use crate::plugin::CompactionContext;
    pub use crate::plugin::ContextCompaction;
    pub use crate::plugin::ContextCompactor;
    pub use crate::plugin::ContextError;
    pub use crate::plugin::ContextPressureContext;
    pub use crate::plugin::ContextPressureDecision;
    pub use crate::plugin::ContextPressureHook;
    pub use crate::plugin::DirectCompletion;
    pub use crate::plugin::DirectLlmCompletion;
    pub use crate::plugin::NoPresentationArtifacts;
    pub use crate::plugin::PersistentRuntimeServices;
    pub use crate::plugin::PluginCommand;
    pub use crate::plugin::PluginExtensionContribution;
    pub use crate::plugin::PluginFactory;
    pub use crate::plugin::PluginHost;
    pub use crate::plugin::PluginLifecycleEvent;
    pub use crate::plugin::PluginLifecycleEventHook;
    pub use crate::plugin::PluginOperation;
    pub use crate::plugin::PluginOperationInvokeError;
    pub use crate::plugin::PluginOperationReceipt;

    pub use crate::plugin::PluginQuery;
    pub use crate::plugin::PluginRegistrar;
    pub use crate::plugin::PluginSession;
    pub use crate::plugin::PluginSessionContext;
    pub use crate::plugin::PluginSessionMaterialization;
    pub use crate::plugin::PluginSessionMaterializationRequest;
    pub use crate::plugin::PluginSessionRequest;
    pub use crate::plugin::PluginSpec;
    pub use crate::plugin::PluginTask;
    pub use crate::plugin::PluginTraceEmitter;

    pub use crate::plugin::HookCause;
    pub use crate::plugin::KeyRejection;
    pub use crate::plugin::PluginStateError;
    pub use crate::plugin::PluginStateView;
    pub use crate::plugin::SessionLifecycleService;
    pub use crate::plugin::SessionObserverIntent;
    pub use crate::plugin::SessionParam;
    pub use crate::plugin::SessionPlugin;
    pub use crate::plugin::StateCommand;
    pub use crate::plugin::StateCommandRefusal;
    pub use crate::plugin::ToolCatalogContribution;
    pub use crate::plugin::ToolPresentationArtifacts;
    pub use crate::plugin::ToolPresentationPresenter;
    pub use crate::plugin::ToolPresentationStep;
    pub use crate::plugin::ToolResultProjectionContext;
    pub use crate::plugin::TurnHookContext;
    pub use crate::plugin::TurnHookReport;
    pub use crate::plugin::TurnResultHookContext;
    pub use crate::plugin::{
        AfterToolContributions, AfterToolDecision, BeforeToolDecision, CachedToolSuccess, HookKey,
        PluginAbort, PluginRecordContribution, PreparedCallReadView, SessionContributions,
        ToolArgsCheckInput, ToolArgsTransformInput, ToolHookContext, ToolHookOccurrence,
        ToolResultCandidate, ToolResultCheckInput, ToolResultTransformInput, TurnContributions,
    };

    pub use crate::plugin::StateReducer;
    pub use crate::plugin::StateReduction;
    pub use crate::plugin::{ModelCallAddress, SessionConfigChangedContext, SessionConfigField};
    pub use crate::plugin::{
        PluginFailureClass, PluginFailureOrigin, PluginHookFailure, PluginOperationFailure,
    };
    pub use crate::plugin::{ToolPresentationFacts, ToolPresentationInput};
    pub use crate::plugin_stack::PluginStack;
    pub use crate::provider::CacheRetention;
    pub use crate::provider::GenerationRetryGuarantee;
    pub use crate::provider::LlmProfileEffortValidationCategory;
    pub use crate::provider::LlmTimeouts;
    pub use crate::provider::Provider;
    pub use crate::provider::ProviderComponents;
    pub use crate::provider::ProviderHandle;
    pub use crate::provider::ProviderOptions;
    pub use crate::runtime::CanonicalRuntimeEffectEnvelope;
    pub use crate::runtime::DirectCompletionClient;
    pub use crate::runtime::EmbeddedRuntimeHost;
    pub use crate::runtime::EventSink;
    pub use crate::runtime::InMemoryLiveReplayStore;
    pub use crate::runtime::InMemoryLiveReplayStoreConfig;
    pub use crate::runtime::LashRuntime;
    pub use crate::runtime::LiveReplayGap;
    pub use crate::runtime::NoopTurnActivitySink;

    pub use crate::runtime::ProcessEngineRegistry;

    pub use crate::runtime::ProcessEventSink;
    pub use crate::runtime::ProcessRuntimeHost;
    pub use crate::runtime::ProcessToolVisibilityFilter;

    pub use crate::runtime::ProcessTurnCancellation;

    pub use crate::runtime::QueuedDrainCandidate;
    pub use crate::runtime::QueuedDrainFamily;
    pub use crate::runtime::QueuedDrainPolicy;
    pub use crate::runtime::QueuedDrainRequest;
    pub use crate::runtime::QueuedDrainSelection;
    pub use crate::runtime::QueuedWorkAuthority;
    pub use crate::runtime::QueuedWorkBatchingConfig;

    pub use crate::runtime::DataRetentionConfig;
    pub use crate::runtime::RuntimeEffectReplayTrace;
    pub use crate::runtime::RuntimeEnvironment;
    pub use crate::runtime::RuntimeEnvironmentBuilder;
    pub use crate::runtime::RuntimeHandle;
    pub use crate::runtime::RuntimeObservation;
    pub use crate::runtime::RuntimeSleepOptions;
    pub use crate::runtime::SessionCommand;
    pub use crate::runtime::SessionCommandReceipt;
    pub use crate::runtime::SessionObservation;
    pub use crate::runtime::SessionObservationSubscription;
    pub use crate::runtime::SessionResume;

    pub use crate::runtime::load_durable_observation_head;
    pub use crate::runtime::{RuntimeDataLimits, RuntimeHostConfig};

    pub use crate::runtime::OperationCancelOutcome;
    pub use crate::runtime::QueueWithdrawalPublisher;
    pub use crate::runtime::SystemClock;
    pub use crate::runtime::TurnActivitySink;
    pub use crate::runtime::TurnAddress;
    pub use crate::runtime::TurnAttach;
    pub use crate::runtime::TurnCancelAffectedInput;
    pub use crate::runtime::TurnCancelInputOutcome;
    pub use crate::runtime::TurnCancelMode;
    pub use crate::runtime::TurnCancelOutcome;
    pub use crate::runtime::TurnCancelReceipt;
    pub use crate::runtime::TurnCancelRequest;
    pub use crate::runtime::TurnCancelUndeliveredInputPolicy;
    pub use crate::runtime::TurnCancellationEvidence;
    pub use crate::runtime::TurnExecutionMetrics;
    pub use crate::runtime::TurnInputAcceptanceReceipt;
    pub use crate::runtime::TurnIssue;
    pub use crate::runtime::TurnIssueSeverity;
    pub use crate::runtime::TurnLaneAdmissionPolicy;
    pub use crate::runtime::TurnTerminal;
    pub use crate::runtime::TurnWorkDriver;

    pub use crate::runtime::WatchedRegistry;
    pub use crate::runtime::WeakRuntimeHandle;
    pub use crate::runtime::current_epoch_ms;

    pub use crate::runtime::validate_replayed_effect_envelope;
    pub use crate::runtime::watch_process_registry;
    pub use crate::runtime::{ParkRefused, ParkedSession};
    pub use crate::runtime::{ProcessChangeHub, ProcessChangeSubscription};
    pub use crate::runtime::{SessionAdministration, SessionDeleteContext, SessionDeleteExecution};
    pub use crate::session::InjectedTurnInput;
    pub use crate::session::ToolInvocation;
    pub use crate::session::ToolInvocationReply;
    pub use crate::session_graph::frame_node_id;
    pub use crate::session_model::ConversationRecord;
    pub use crate::session_model::GenerationOverlay;

    pub use crate::session_model::context::PreparedContext;
    pub use crate::store::{CommitBudget, CommitBudgetLimit};
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use crate::tool_registry::ReconfigureError;
    pub use lash_core_execution::Response;
    pub use lash_core_store::session_graph::facade_ops::{
        SessionGraphFacadeOps, SessionNodeProjection,
    };
    pub use lash_core_store::session_identity::facade_ops::AgentFrameReasonFacadeOps;
    pub use lash_core_store::session_state::facade_ops::RuntimeSessionStateFacadeOps;
    pub use lash_protocol_types::AcceptedInjectedTurnInput;
    pub use lash_protocol_types::AttachmentMaterializationNotice;
    pub use lash_protocol_types::AttachmentMaterializationReason;
    pub use lash_protocol_types::AttachmentRef;
    pub use lash_protocol_types::ErrorEnvelope;
    pub use lash_protocol_types::MessageSequence;
    pub use lash_protocol_types::ModelToolReturnPart;
    pub use lash_protocol_types::ProviderSchemaCapabilities;
    pub use lash_protocol_types::ReportedFailure;
    pub use lash_protocol_types::ResolvedSchema;
    pub use lash_protocol_types::RetryProgress;
    pub use lash_protocol_types::SchemaPurpose;
    pub use lash_protocol_types::SchemaResolutionError;
    pub use lash_protocol_types::SchemaResolutionRequest;
    pub use lash_protocol_types::SessionStreamEvent;
    pub use lash_protocol_types::StreamMessageKind;
    pub use lash_protocol_types::ToolCatalogBuildError;
    pub use lash_protocol_types::TurnFinish;
    pub use lash_protocol_types::TurnOutcome;
    pub use lash_protocol_types::append_assistant_text_part;
    pub use lash_protocol_types::build_tool_catalog;
    pub use lash_protocol_types::normalized_response_parts;
    pub use lash_protocol_types::reasoning_part;
    pub use lash_protocol_types::resolve_schema;
    pub use lash_protocol_types::shared_parts;
    pub use lash_protocol_types::visible_response_text_from_parts;
    pub use lash_protocol_types::{ModelToolOutcome, ModelToolReturn};
    pub use lash_protocol_types::{PluginStage, TurnErrorCause, TurnErrorCode, TurnStop};
    pub use lash_sansio::EffectId;
    pub use lash_trace::TraceBranchSelection;
    pub use lash_trace::TraceDetailPolicy;
    pub use lash_trace::TraceRecord;
    pub use lash_trace::TraceRuntimeScope;
    pub use lash_trace::TraceRuntimeSubject;
    pub use lash_trace::TraceSink;
    pub use lash_trace::TraceSinkError;
    /// The schemars crate first-party config owners derive their schemas
    /// through (`#[schemars(crate = "lash_core::facade_support::schemars")]`).
    pub use schemars;
    pub use schemars::JsonSchema;
}

pub(crate) use facade_support::*;

// `facade_support` is the workspace's internal cross-crate seam, and membership
// in it means some crate's *shipped* code needs the item (FIG-1223). These
// twelve had test-only consumers, so their public path is `test_support` and
// only their crate-internal short path lives here: `test_support` is
// feature-gated and `crate::X` has to resolve in every build.
pub(crate) use crate::plugin::RuntimeServices;

pub mod sansio {
    pub use crate::{CompletedToolCall, Response};
    pub(crate) use lash_sansio::sansio::LogEvent;
    pub use lash_sansio::sansio::{
        ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, ContextProjector,
        EffectId, ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure,
        ExecutionEnvironmentSyncFailureKind, ExpandedRow, ExpandedWrapper, ModelToolCalls,
        PendingToolCall, PendingWork, ProtocolDriverHandle, ResponseToolCalls, SyncedEnvironment,
        ToolExpansionPlan, ToolWave, TurnMachine, place_prompt,
    };
}

pub use attachments::{
    AttachmentBlobPage, AttachmentGcFence, AttachmentPolicy, AttachmentReadPolicy,
    AttachmentReclamationPolicy, AttachmentRootSet, AttachmentStore, AttachmentStoreError,
    AttachmentStoreFailureClass, AttachmentStorePersistence, EmptyRootSetPolicy, StoredAttachment,
    StoredBlobRef,
};
pub use lash_core_execution::CALL_DYNAMIC_EFFECT;
pub use lash_core_execution::DeferredMatch;
pub use lash_core_execution::DeferredOutcome;
pub use lash_core_execution::DeferredResolutionPolicy;
pub use lash_core_execution::DeferredResolveContext;
pub use lash_core_execution::DeferredToolProvider;
pub use lash_core_execution::DeferredToolProviderRegistration;
pub use lash_core_execution::DynamicCall;
pub use lash_core_execution::EffectLink;
pub use lash_core_execution::EffectName;
pub use lash_core_execution::ExecutionBinding;
pub use lash_core_execution::ExecutionBindingUnreadable;
pub use lash_core_execution::LinkKind;
/// The run spec a frame switch's follow-on runs under (FIG-6021).
pub use lash_core_execution::NextRun;
pub use lash_core_execution::ObligationToken;
pub use lash_core_execution::turn_stop_from_tool_control;

pub use lash_core_execution::ProcessSurface;
pub use lash_core_execution::ProviderKey;
pub use lash_core_execution::RecordedSurface;
pub use lash_core_execution::RegistryKind;
pub use lash_core_execution::ResolvedLink;
pub use lash_core_execution::SurfaceCollision;
pub use lash_core_execution::SurfaceDuplicateTool;
pub use lash_core_execution::SurfaceEntry;
pub use lash_core_execution::SurfaceRefusal;
pub use lash_core_execution::SurfaceTools;
pub use lash_core_execution::ToolSurfaceDrift;
pub use lash_core_execution::ToolSurfaceDriftKind;
pub use lash_core_execution::call_dynamic_endpoint;
pub use lash_core_execution::effect_path_of;
pub use lash_protocol_types::llm::attachment_delivery::ProviderFileScope;
pub use lash_protocol_types::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, Backoff, BatchId,
    BindingChanges, BoundedRetry, CallResultSummary, CancelOrigin, CancelRequest, CellControl,
    CellDefect, CellFailure, CellFailureKind, CellFailureRepair, CellOutcome, CellPrint,
    CellRecord, CheckpointKind, CompletionCandidate, CompletionDisposition, DecidedCompletion,
    DeclarationRefusal, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason, ExecResponse,
    ExecutedCall, ExecutedCallOutcome, ExecutionBudgets, ExecutionBudgetsConfig,
    ExecutionBudgetsError, ExecutionLimit, ExecutionPolicy, FinishDeclaration, FrameKey,
    FrameKeyError, InputId, InternalPartKind, JsonSchema, LimitCause, LlmCallError, LlmUsage,
    MediaType, Message, MessageOrigin, MessageRole, NodeId, OutcomeShape, OutputRetentionPolicy,
    OutputValue, ParkBound, Part, PartKind, PluginMessage, PluginRuntimeEvent, ProjectionMode,
    ProviderAttemptLimits, RegistrationRefused, RetainedOutput, RetainedValue, RunId,
    SchemaAdmissionError, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SessionAppendNode, TerminationMode, TextProjectionMetadata,
    TokenUsageOverflow, ToolArgumentProjectionPolicy, ToolBinding, ToolBound, ToolBounds,
    ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolCancellation, ToolCatalog,
    ToolCatalogBuildError, ToolCatalogEntry, ToolCheckConflict, ToolCheckPhase, ToolCheckReply,
    ToolCheckVerdictKind, ToolContract, ToolControl, ToolDeclaration, ToolDefinition,
    ToolDefinitionBindingExt, ToolDiscovery, ToolDraft, ToolFailure, ToolFailureCause,
    ToolFailureClass, ToolFailureSource, ToolId, ToolIntentIdentity, ToolIntentKind, ToolManifest,
    ToolModule, ToolOutputContract, ToolValue, ToolView, ToolViewBlock, ToolViewMeta, TurnControl,
    TurnControlKind, TurnControls, TurnId, TurnOutputSource, TurnReply, ValueMismatch, ValueShape,
    ValueShapeLimits,
};
pub(crate) use lash_protocol_types::{
    BaseRenderCache, messages_are_prompt_resume_safe, visible_response_parts,
};
pub use lash_protocol_types::{
    llm::types::AttemptOutcome, llm::types::AttemptRecord, llm::types::AttemptUsageOutcome,
    llm::types::ChargeSafetyDecision, llm::types::ChargeSafetyDenialReason,
    llm::types::DiscardReason, llm::types::DiscardedAfterCell, llm::types::ExecutionEvidence,
    llm::types::ExecutionEvidenceCollectionInterruption, llm::types::ExecutionEvidenceMergeError,
    llm::types::GenerationOptionOutcome, llm::types::GenerationOptions,
    llm::types::GenerationReceipt, llm::types::LlmCallId, llm::types::LlmCallRecord,
    llm::types::LlmOutputPart, llm::types::LlmRequest, llm::types::LlmRequestOwner,
    llm::types::LlmRequestScope, llm::types::LlmResponse, llm::types::LlmStreamEvidence,
    llm::types::LlmTerminalReason, llm::types::LlmTurnScope, llm::types::NonNegativeFiniteF64,
    llm::types::NormalizedError, llm::types::ProtocolPosition, llm::types::ProviderEndpointError,
    llm::types::ProviderReplayDrop, llm::types::ProviderReplayDropReason,
    llm::types::ProviderReplayKind, llm::types::ProviderRouteIdentity,
    llm::types::RecordedRequestTemplate, llm::types::ResponseContext, llm::types::ResponseContract,
    llm::types::RetryClass, llm::types::RetryDecision, llm::types::RetryDeclineCause,
    llm::types::RetryWait, llm::types::ToolCallContract,
};
pub use lash_sansio::CheckpointDelivery;
pub(crate) use lash_sansio::build_turn;
pub use protocol_build::ProtocolBuildInput;
pub use tool_provider::{ToolAttachmentClient, ToolDirectCompletionClient, ToolSessionLlmProfile};
pub use tool_registry::ToolRegistry;
pub use tool_result::{CancelHint, PendingCompletion, PendingResolver, ToolOutcome};

pub(crate) mod facade_ops {}
pub use lash_core_execution::ArtifactCarry;
pub use lash_core_execution::ArtifactCleanup;
pub use lash_core_execution::ArtifactName;
pub use lash_core_execution::ArtifactReferrer;
pub use lash_core_execution::ArtifactReferrerError;
pub use lash_core_execution::ArtifactReferrerKind;
pub use lash_core_execution::ArtifactStoreId;
pub use lash_core_execution::AttachmentUploadId;
pub use lash_core_execution::FrameEnvironmentId;
pub use lash_core_execution::HostArtifactPin;
pub use lash_core_execution::ReferrerClaim;
pub use lash_core_execution::ReferrerGuard;
pub use lash_core_execution::ReferrerStore;
pub use lash_core_execution::ResolvedArtifactCleanup;
pub use lash_core_execution::RuntimeOwner;
pub use lash_core_execution::UploadReferrerId;

pub use lash_core_execution::DurableConfig;
pub use lash_core_execution::DurableSettings;
pub use lash_core_execution::DurableStore;

pub use lash_core_execution::ModuleArtifactCorruption;

pub use lash_core_execution::ModuleArtifactRefusal;

pub use lash_core_execution::{
    DriverAction, DriverContextView, Effect, HostTurnProtocol, PreparedTurnMachine,
    ProjectorContext, ProtocolDriverState, SansIoTurnInput, TurnDriverConfig, TurnDriverPreamble,
    TurnMachine, TurnMachineConfig,
};
pub use lash_protocol_types::{
    InvalidToolCallId, ToolCallAdmission, ToolCallId, ToolCallPosition, ToolCallRoot,
    ToolCallRootError,
};
pub use lash_sansio::{
    BuildNewestWriterFormats, WriterFormats, build_newest_writer_formats, driver_writer_version,
};

pub use lash_protocol_types::{
    FailureCode, HostNamespace, InvalidNamespace, Namespace, TurnFailureCode, TurnFailureKind,
};
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{
    LashTracerProvider, OtelOptions, OtelTelemetry, SpanEnricher, SpanEnrichment, SpanView,
};
pub use lash_trace::{
    AmbientContext, AmbientGuard, AmbientScope, AttrKey, ExtensionAttribute, ExtensionContent,
    ExtensionOperation, ExtensionSpanEnd, ExtensionSpanEvent, ExtensionSpanKind,
    ExtensionSpanRecord, SpanAttributes, SpanSpec, ToolSpanAnnotations, TraceValue,
};
pub use lash_trace::{
    ContentPolicy, DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier,
    InvalidTraceLinks, LashIdentity, ProducerKind, ProducerOutcome, ProducerSpan, ProducerSpec,
    ProductObservationSink, SpanKey, SpanRole, TraceAttemptId, TraceAttemptLink, TraceCall,
    TraceCaller, TraceCarrier, TraceCause, TraceIdentity, TraceIds, TraceLinkMode, TraceLinks,
    TraceParent, TracePosition, TraceScopeAdmission, TraceScopeId, TraceScopeKind, TraceScopeOffer,
    TraceScopeOwner, TraceSpanKind, TraceTimestamp, W3cSpanId, W3cTraceFlags, W3cTraceId,
    W3cTraceState, derive_span,
};
pub use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEffectEnvelopeDiffEntry,
    TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue, TraceError, TraceEvent,
    TraceLlmMessage, TraceLlmRequest, TraceLlmResponse, TracePromptComponent,
    TraceProviderBodyOmission, TraceProviderEvent, TraceProviderReplayDropEvent,
    TraceProviderReplayDropReason, TraceProviderReplayKind, TraceProviderRouteIdentity,
    TraceRuntimeStreamEvent, TraceRuntimeStreamPayload, TraceToolResultBlock, TraceToolSpec,
};
pub use llm::transport::ProviderFailureKind;
pub use llm_profile::{
    LlmProfileConfig, LlmProfileKey, LlmProfileLimits, LlmProfileLimitsError, LlmProfileMetadata,
    LlmProfileMetadataBuilder, OutputTokenLimits, ReasoningRefused, RecordedLlmProfile,
};
pub use plugin::AdmittedPluginConfig;
pub use plugin::CandidateFacts;
pub use plugin::ConfigCommand;
pub use plugin::ConfigCommandCatalog;
pub use plugin::ConfigCommandDescriptor;
pub use plugin::ConfigOwner;
pub use plugin::ConfigRegistrar;
pub(crate) use plugin::PluginRuntimeDirective;

pub use plugin::ConfigRegistry;
pub use plugin::ConfigSubmitError;
pub use plugin::ConfigTransaction;
pub use plugin::ConfigWire;
pub use plugin::CoreConfigOwner;

pub use plugin::AgentFrameAssignment;
pub use plugin::AgentFrameReason;
pub use plugin::AgentFrameRecord;
pub use plugin::AppendSessionNodesOutcome;
pub use plugin::AppendSessionNodesRequest;
pub use plugin::CreationConfigError;
pub use plugin::FormatNamespace;
pub use plugin::FormatRefusal;
pub use plugin::FormatVersion;
pub use plugin::FrameNodeId;
pub use plugin::FrameNodeIdError;
pub use plugin::KeyRejection;
pub use plugin::NoRunOptions;
pub use plugin::OwnerChange;
pub use plugin::OwnerConfigView;
pub use plugin::PluginConfig;
pub use plugin::PluginConfigNamespace;

pub use plugin::PluginExtensions;
pub use plugin::PluginNamespaceState;
pub use plugin::PluginOptions;
pub use plugin::PluginState;
pub use plugin::PluginStateEffect;
pub use plugin::PluginStateError;
pub use plugin::PluginStateView;
pub use plugin::PluginTransitionBase;
pub use plugin::PluginTransitionId;
pub use plugin::PluginTransitionRecord;
pub use plugin::PluginTransitionRequest;
pub use plugin::ProcessEngineContributionContext;
pub use plugin::ProtocolBeforeLlmCallContext;
pub use plugin::ProtocolLlmCallAction;

pub use plugin::SessionGraphService;
pub use plugin::SessionLineage;
pub use plugin::SessionReadView;
pub use plugin::SessionRelation;
pub use plugin::SessionSnapshot;
pub use plugin::SessionStartPoint;
pub use plugin::SessionStateService;

pub use plugin::{OpenAgentFrameOutcome, OpenAgentFrameRequest};

pub use lash_core_execution::{
    FinishToolProvider, finish_tool, finish_tool_id, suppress_default_finish,
};

pub use provider::{
    AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
    AttachmentCapabilitySnapshot, CacheControlDialect, GoogleDialect, InstructionRole,
    LlmProfileCapability, OpenAiReasoningContext, ReasoningCapability, ReasoningEncoding,
    ReasoningIntent, ReasoningRetentionCapability, ReasoningRetentionPolicy,
    ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
    ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability, StreamTermination,
};
pub use provider::{
    EmptyLlmProfiles, LlmProfileRegistry, LlmProfileUnavailable, LlmProfileUnavailableReason,
    LlmProfiles, RegisteredLlmProfile, RegistrationError,
};
pub(crate) use provider::{ProviderCompletion, ProviderCompletionError};

#[cfg(any(test, feature = "testing"))]
pub use runtime::{ObservationSource, work_with_observations};

// This block includes the effect / process-control types consumed by host
// process engines and their integration tests —
// they are deliberately public; the rest of the runtime module stays
// crate-internal.
/// A host's successor pair for a process, and how an engine carries a
/// process onto one (FIG-6121, D2).
pub use lash_core_execution::request_process_successor;
/// The artifact ports a process-engine registry acquires start and revision
/// artifacts through (ADR 0113 §3.3), for hosts that assemble a registry
/// outside `RuntimeHostConfig`.
pub use lash_core_execution::runtime::{ArtifactReferrerPorts, ReferrerAcquisition};

pub use runtime::ActiveTurnIngress;

pub use runtime::AdmittedScope;
pub use runtime::AdmittedTurnInputs;

pub use runtime::AssistantResponseHookEvents;
pub use runtime::AssistantResponsePlan;
pub use runtime::AssistantStreamHookState;
pub use runtime::AwaitEventKey;
pub use runtime::AwaitEventWaitIdentity;
pub use runtime::BindingId;
pub use runtime::CapabilityRef;

pub use runtime::CausalRef;
pub use runtime::ChargeSafetyRefusalEvidence;
pub use runtime::CheckpointAdmittedSet;
pub use runtime::Clock;
pub use runtime::ClockWallTime;
pub use runtime::CommandJournalGuard;
pub use runtime::CommandReplayKey;
pub use runtime::ContractRef;

pub use runtime::DefinitionRef;
pub use runtime::DeliveryPolicy;

pub use runtime::DeploymentStoreDecorator;
pub use runtime::DrainMode;
pub use runtime::DrainModePolicy;
pub use runtime::DurableProcessWork;
pub use runtime::EffectAddress;
pub use runtime::EffectJournalRetirement;
pub use runtime::EffectOpener;
pub use runtime::EffectOpenerError;
pub use runtime::EffectRetirementGate;
pub use runtime::EnclosingProcess;

pub use runtime::ExecutableGeneration;
pub use runtime::ExecutableGenerationRefusal;
pub use runtime::ExecutionScope;

pub use runtime::ForkSessionReceipt;
pub use runtime::ForkSessionRequest;
pub use runtime::HandleId;
pub use runtime::HoldEnvError;

pub use runtime::InputItem;

pub use runtime::InvalidProcessDefinitionId;
pub use runtime::InvalidStartKey;
pub use runtime::JournalReplay;

pub use runtime::LiveReplayEventDraft;
pub use runtime::LiveReplayGapReason;
pub use runtime::LiveReplayOutcome;
pub use runtime::LiveReplayStore;
pub use runtime::LiveReplayStoreError;
pub use runtime::LiveReplaySubscribeOutcome;
pub use runtime::LiveReplaySubscription;
pub use runtime::LlmRequestSpec;
pub use runtime::LlmStreamRecord;

pub use runtime::NoProcessWork;
pub use runtime::NoRunOptionsOwner;

pub use runtime::OwnerRunOptions;

pub use runtime::PendingTurnInput;
pub use runtime::PendingTurnInputBatch;
pub use runtime::PendingTurnInputCancelOutcome;
pub use runtime::PendingTurnInputCancelReceipt;
pub use runtime::PendingTurnInputCancelTarget;
pub use runtime::PendingTurnInputDraft;
pub use runtime::PendingTurnInputRead;
pub use runtime::PendingTurnInputReadStatus;
pub use runtime::PendingTurnInputSuffixCancelOutcome;

pub use runtime::ProcessCommand;

pub use runtime::ProcessComponentConfig;
pub use runtime::ProcessComponentEntry;
pub use runtime::ProcessComponentId;
pub use runtime::ProcessComponentIdError;
pub use runtime::ProcessConfigKey;

pub use runtime::ProcessDefinitionId;

pub use runtime::ProcessDriveStep;

pub use runtime::ProcessEffectOutcome;

pub use runtime::ProcessEngineRegistration;
pub use runtime::ProcessEngineRegistry;
pub use runtime::ProcessEnv;
pub use runtime::ProcessEnvBuilder;
pub use runtime::ProcessEnvChoice;
pub use runtime::ProcessEnvCx;
pub use runtime::ProcessEnvPolicy;
pub use runtime::ProcessEnvPolicyError;
pub use runtime::ProcessEnvPorts;
pub use runtime::ProcessEnvRef;
pub use runtime::ProcessEnvRefusal;
pub use runtime::ProcessEnvSelection;

pub use runtime::ProcessId;
pub use runtime::ProcessIdMint;

pub use runtime::ProcessLimits;

pub use runtime::ProcessListSelection;

pub use runtime::ProcessModel;

pub use runtime::ProcessOpScope;

pub use runtime::ProcessOutcomeObserver;

pub use runtime::ProcessRegistryAwaiter;

pub use runtime::ProcessService;

pub use runtime::ProcessStartChoice;

pub use runtime::ProcessTerminalWait;

pub use runtime::ProcessWorkSubstrate;
pub use runtime::ProcessWorkWiring;

pub use runtime::ProtocolSessionExtension;
pub use runtime::QueuedDrainCandidate;
pub use runtime::QueuedDrainFamily;
pub use runtime::QueuedDrainPolicy;
pub use runtime::QueuedDrainRequest;
pub use runtime::QueuedDrainSelection;
pub use runtime::QueuedWorkAuthority;
pub use runtime::QueuedWorkBatchingConfig;
pub use runtime::RecordedKeyFence;
pub use runtime::RecordedKeyRange;
pub use runtime::RecordedKeys;
pub use runtime::RecordedRefusal;
pub use runtime::RecordedRender;
pub use runtime::RefusedWriteRange;
pub use runtime::RenderFault;
pub use runtime::RenderRefusal;
pub use runtime::Resolution;
pub use runtime::ResolveOutcome;
pub use runtime::ResolvedInlineStart;

pub use runtime::ResolvedRun;
pub use runtime::ResolvedStartTarget;
pub use runtime::RetainedRevision;
pub use runtime::Retention;
pub use runtime::RunAggregateWakePolicy;
pub use runtime::RunDefinition;
pub use runtime::RunDefinitionRefusal;
pub use runtime::RunDefinitions;
pub use runtime::RunOptionsEncodeError;
pub use runtime::RunOptionsKey;
pub use runtime::RunOptionsOwner;
pub use runtime::RunOverrides;
pub use runtime::RunResolveError;
pub use runtime::RunShapeRefusal;
pub use runtime::RunSpec;
pub use runtime::RunSpecHash;
pub use runtime::RuntimeAttribution;
pub use runtime::RuntimeCheckpointComponents;
pub use runtime::RuntimeEffectCommand;
pub use runtime::RuntimeEffectControllerError;
pub use runtime::RuntimeEffectEnvelope;
pub use runtime::RuntimeEffectInvocation;
pub use runtime::RuntimeEffectKind;
pub use runtime::RuntimeEffectLocalExecutor;
pub use runtime::RuntimeEffectOutcome;
pub use runtime::RuntimeEffectReplayMismatchReport;
pub use runtime::RuntimeError;
pub use runtime::RuntimeErrorCause;
pub use runtime::RuntimeErrorCode;
pub use runtime::RuntimeInvocation;
pub use runtime::RuntimeReplay;
pub use runtime::RuntimeReplayAttribution;
pub use runtime::RuntimeSessionAuthority;
pub use runtime::RuntimeSessionState;
pub use runtime::SCOPE_STORAGE_PAYLOAD_VERSION;

pub use runtime::ScopeId;

pub use runtime::ScopeStorageError;
pub use runtime::SegmentProgress;
pub use runtime::ServedOnly;
pub use runtime::ServedOnlyRange;
pub use runtime::SessionAdministration;
pub use runtime::SessionCreationHead;
pub use runtime::SessionCursor;
pub use runtime::SessionCursorError;
pub use runtime::SessionDeleteContext;
pub use runtime::SessionDeleteExecution;
pub use runtime::SessionEntry;
pub use runtime::SessionId;
pub use runtime::SessionListFilter;
pub use runtime::SessionObservationEvent;
pub use runtime::SessionObservationEventPayload;
pub use runtime::SessionProcessEventKind;
pub use runtime::SessionQueueEventKind;
pub use runtime::SessionRelationKind;
pub use runtime::SessionRevision;

pub use runtime::SessionStateVersionRefusal;
pub use runtime::SessionStoreCreateRequest;
pub use runtime::SessionView;
pub use runtime::SleepSpec;
pub use runtime::SlotId;
pub use runtime::StagedProcessStart;

pub use runtime::StartKey;

pub use runtime::StoredDataCorruption;
pub use runtime::SubmittedBody;
pub use runtime::Target;
pub use runtime::ToolAttemptLaunch;
pub use runtime::ToolIntentActivityOutcome;
pub use runtime::ToolIntentRealizedRef;
pub use runtime::TurnActivity;
pub use runtime::TurnActivityId;
pub use runtime::TurnCancelAffectedInput;
pub use runtime::TurnCancelInputOutcome;
pub use runtime::TurnCancelMode;
pub use runtime::TurnCancelUndeliveredInputPolicy;
pub use runtime::TurnCancelWait;
pub use runtime::TurnContext;
pub use runtime::TurnEvent;
pub use runtime::TurnFailureCause;
pub use runtime::TurnFailureEvidence;
pub use runtime::TurnFailurePartialOutput;
pub use runtime::TurnFailureSettlement;
pub use runtime::TurnInput;
pub use runtime::TurnInputAdmissionMode;
pub use runtime::TurnInputApplication;
pub use runtime::TurnInputCheckpointBoundary;
pub use runtime::TurnInputCompletion;
pub use runtime::TurnInputCompletionData;
pub use runtime::TurnInputIngress;
pub use runtime::TurnInputState;
pub use runtime::TurnInputStateKind;
pub use runtime::TurnLaneAdmissionPolicy;
pub use runtime::TurnPrelude;

pub use runtime::WatchedRegistry;
pub use runtime::WeakProcessEngineRegistry;
pub use runtime::WorkCadenceError;
pub use runtime::WorkCadencePolicy;

pub use runtime::hold_process_env;

pub use runtime::mint_process_id;

#[allow(unused_imports)]
pub(crate) use runtime::AdmissionBoundary;
pub(crate) use runtime::AdmittedQueuedWork;

pub(crate) use runtime::QueuedWorkBatch;
pub(crate) use runtime::QueuedWorkBatchDraft;
pub(crate) use runtime::QueuedWorkCompletion;
pub(crate) use runtime::QueuedWorkEnqueueOutcome;

/// Process observation: the snapshot, cursor, stream events and bounded
/// live replay of one process (D-PROCOBS).
pub use runtime::{
    InMemoryProcessReplayStore, InMemoryProcessReplayStoreConfig, LanguageExecutionObservation,
    ParsedProcessObservationCursor, ProcessDocumentIdentity, ProcessEffectCoverage,
    ProcessEffectEvidence, ProcessEffectGapReason, ProcessObservation, ProcessObservationCursor,
    ProcessObservationCursorError, ProcessObservationEnd, ProcessObservationEvent,
    ProcessObservationEventPayload, ProcessObservationGapCause, ProcessObservationIdentity,
    ProcessObservationReplacement, ProcessReadView, ProcessReplayEventDraft,
    ProcessReplayGapReason, ProcessReplayOutcome, ProcessReplayPublishLimits, ProcessReplayStore,
    ProcessReplayStoreError, ProcessReplaySubscribeOutcome, ProcessReplaySubscription,
    ProcessSequence, ProcessWorkflowFoldError, ProcessWorkflowLiveCoverage,
    ProcessWorkflowObservation, ProcessWorkflowOverlay, ProcessWorkflowOverlayAccumulator,
    ProcessWorkflowRan, ProcessWorkflowSite, ProcessWorkflowSiteState, ProcessWorkflowWaitKind,
    RetainedProcessView, StepBodyStartedObservation, commits_bridge,
};

pub(crate) use session_model::plugin_runtime_protocol_event;

pub(crate) use session::RuntimeExecutionTracing;
pub(crate) use session::Session;
pub use session::{
    CellEndHandoff, EndedCell, ExecRequest, ExecutionEnvironmentSyncError, HeldResponse,
    RuntimeExecutionContext, SessionError, ToolDispatchSurface, tool_dispatch_surface,
};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SESSION_NODE_BODY_SCHEMA_VERSION, SessionGraph,
    SessionNodePayload, SessionNodeRecord, UndeliveredConfigChange,
};

/// Declared reads (ADR 0144): what answers a `Read` tool's calls inside a
/// code cell.
pub use lash_core_execution::{
    ReadCall, ReadError, ReadRoute, ToolProviderRegistration, ToolReads,
};
pub use session_model::LlmProfileBinding;
pub use session_model::{
    ChargeSafetyPolicy, MaxToolCalls, NoProgressBudget, SessionPolicy, ToolCallLimitExceeded,
    ToolCallLimitScope, TurnBudget,
};
pub use session_model::{ProtocolEvent, ProtocolPayload, SessionHistoryRecord};
pub use store::{
    AdoptedAttachmentCondemnation, AppendRequestIdentity, AttachmentCondemnation,
    AttachmentCondemnationAdoption, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentDeleteArming,
    AttachmentDeleteStallReason, AttachmentReferrers, AttachmentSettlementOutcome,
    AttachmentSweepGeneration, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit,
    AttachmentWriteToken, BlobRef, CURRENT_SESSION_STATE_VERSION, CheckpointComponentDescriptor,
    CheckpointExecution, CommitBudget, CommitBudgetLimit, DurableItem, DurablePayload, DurableScan,
    DurableScanPage, DurableSurface, FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState,
    FleetFormatStore, GcReport, HydratedCheckpointComponent, HydratedSessionCheckpoint,
    LeaseIncarnationId, LeaseOwnerId, LeaseOwnerIdentity, MAX_ATTACHMENT_DELETE_ATTEMPTS,
    MaintenanceFailure, MaintenanceRefusal, MaintenanceReport, MaintenanceResult, MaintenanceStop,
    MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION, OperationId, QueuedWorkStore,
    RetentionBound, RetentionReport, RuntimeCommit, RuntimeStore, RuntimeStoreDecorator,
    RuntimeTurnCommitStamp, ScanCoverage, SemanticBoundaryOperation, SessionAdmission,
    SessionBlobReclaimReport, SessionCatalogStore, SessionCommitStore, SessionHistoryStore,
    SessionLookup, SessionMeta, SessionReferrerState, SessionStateAdmission, SessionStore,
    StoreBackend, StoreComponentVersion, StoreError, StoreMaintenance, StorePreflight,
    StoreReleaseStamp, StoreReleaseState, StoreSchemaDatabase, StoreSchemaOutcome,
    StoreSchemaStatus, StoreSchemaVerdict, SurfaceFormat, TurnInputAdmission, TurnInputForsaking,
    TurnInputStore, VacuumReport, WriterPin, compare_releases, release_stamp_advances,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphAppend, RuntimeCommitReceipt, SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
    ensure_supported_schema_version,
};

pub use lash_core_execution::ToolAttemptOutcome;

pub use lash_core_execution::ToolOutcomeDone;

/// Tool-provider contracts, including child-process execution observation hooks.
pub use tool_provider::{
    AttemptContext, AttemptProcessReads, AttemptSessionReads, IsolatedProcessRequest,
    IsolatedStart, PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall, ResolveTargetError,
    ToolCall, ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolExecutionGrant,
    ToolPrepareCall, ToolPrepareContext, ToolProvider,
};
#[doc(hidden)]
pub mod core_internal {
    pub use crate::runtime::{
        EngineProcessRuntime, ProcessRuntimePorts, ProcessStepTools, RuntimeSessionServices,
        SessionTurnRuntime,
    };
    pub use lash_core_execution::core_internal::RuntimeService;
    pub use lash_core_execution::core_internal::{
        RuntimeEffectLocalRunner, RuntimeExecutionContextRuntimeOps, StartKeyDerivation,
        owned_runner_executor,
    };
}

pub use lash_core_execution::AttachmentContentMismatch;
pub use lash_core_execution::AttachmentRetentionFailure;
pub use lash_core_execution::AttachmentRetentionStoreFailure;
pub use lash_core_execution::CompletedToolCall;
pub use lash_core_execution::Response;

pub use lash_core_execution::{EffectAttempt, RecordedEffectExecution};

#[cfg(test)]
mod attachments_tests;

pub use lash_core_store::transcript;

pub use lash_core_execution::tool_live::{
    CallAddress, TOOL_LIVE_ENVELOPE_BYTES, ToolLive, ToolLiveCursor, ToolLiveDiagnostics,
    ToolLiveEmission, ToolLiveEmissionId, ToolLiveError, ToolLiveFact, ToolLiveFaultSource,
    ToolLiveFilter, ToolLiveGapReason, ToolLiveItem, ToolLiveLogLevel, ToolLiveMemoryWindow,
    ToolLiveOmission, ToolLiveOwnerOmission, ToolLivePayload, ToolLivePolicy, ToolLivePolicyBound,
    ToolLivePolicyError, ToolLivePolicyField, ToolLivePolicyRelation, ToolLivePolicySnapshot,
    ToolLivePolicyValue, ToolLivePublisher, ToolLiveReadFault, ToolLiveRecord, ToolLiveReplay,
    ToolLiveRetention, ToolLiveSubscription,
};

pub use lash_core_execution::tool_live;

/// Common statement counts supplied to the workflow coverage helper.
pub use lash_kernel_doc::RanCounts;

pub use lash_core_store::model_calls;
