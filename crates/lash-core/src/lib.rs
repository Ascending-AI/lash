//! Runtime kernel for Lash.
//!
//! The process kernel understands `SessionTurn` to coordinate child-session
//! turns. Executable process bodies, including work a host runs outside lash,
//! use `ProcessInput::Engine { kind, payload }` and call ordinary recorded tool
//! attempts under their process journal.
//!
//! Protocols follow the same boundary: core owns the `HostTurnProtocol` state
//! shape and the `ProtocolDriverPlugin` slot, while external protocol crates
//! provide the driver implementation.

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
pub use lash_core_execution::formats;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_execution::process_id_for_test;
pub use lash_core_execution::process_id_from_handle_json;
pub use lash_core_execution::runtime::actor::round::{
    CallOwner, ParkedCall, StagedPluginState, StoreLocalEffect, StoreLocalRows, parked,
};
/// Durable tool-effect format versions, re-exported for the format manifest.
pub use lash_core_execution::waits;
pub use lash_core_execution::{
    NoProjectionProviders, PinnedKey, ProjectionProviders, ResolveAnswer,
};
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
    pub(crate) use lash_core_ids::panic_containment::{enforce_message, payload_message};
    pub use lash_core_ids::panic_containment::{is_loud, set_loud};
}
pub use lash_core_execution::hook_key;
pub use lash_core_execution::plugin;
pub(crate) use lash_core_execution::plugin_stack;
pub(crate) use lash_core_execution::protocol_build;
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
    pub use lash_sansio::sync::*;
}
#[cfg(any(test, feature = "testing"))]
pub use lash_core_execution::test_support;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_ids::test_watchdog;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use lash_core_execution::tool_dispatch;
pub(crate) use lash_core_execution::tool_intent;
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
    pub use lash_core_execution::facade_support::observe_process_records;
    pub use lash_core_execution::runtime::process::steps::read_process_tool_call;
    /// The shift-tracing seam a durable substrate implements against (the trace runtime,
    /// a step's issue and its standing), public in every feature variant.
    pub use lash_core_execution::trace::{
        JournalFrontier, LiveStep, StepIssue, TraceBoundaryReceipt, TraceRuntime, TraceStanding,
    };
    pub use lash_core_ids::operational_metrics::StoreObserver;
    pub use lash_core_llm::core_internal::ProviderCompletionSideband;
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
    pub fn resolve_tool_registry_contract(
        registry: &crate::ToolRegistry,
        name: &str,
    ) -> Option<std::sync::Arc<crate::ToolContract>> {
        registry.resolve_catalog_contract(name)
    }

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
    pub use crate::llm::transport::LlmTransportError;
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
    pub use crate::plugin::PluginOwned;
    pub use crate::plugin::PluginQuery;
    pub use crate::plugin::PluginRegistrar;
    pub use crate::plugin::PluginSession;
    pub use crate::plugin::PluginSessionContext;
    pub use crate::plugin::PluginSessionMaterialization;
    pub use crate::plugin::PluginSessionMaterializationRequest;
    pub use crate::plugin::PluginSessionRequest;
    pub use crate::plugin::PluginSpec;
    pub use crate::plugin::PluginSpecFactory;
    pub use crate::plugin::PluginTask;
    pub use crate::plugin::PluginTraceEmitter;
    pub use crate::plugin::SessionConfigChangedContext;
    pub use crate::plugin::SessionHandle;
    pub use crate::plugin::SessionLifecycleService;
    pub use crate::plugin::SessionObserverIntent;
    pub use crate::plugin::SessionParam;
    pub use crate::plugin::SessionPlugin;
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
        ToolMembershipContribution, ToolResultCandidate, ToolResultCheckInput,
        ToolResultTransformInput, TurnContributions,
    };
    pub use crate::plugin::{
        HookCause, KeyRejection, PluginStateError, PluginStateView, StateCommand,
        StateCommandRefusal, StateCommands, StateReducer, StateReduction,
    };
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
    pub use crate::runtime::AgentFrameRun;
    pub use crate::runtime::AssembledTurn;
    pub use crate::runtime::CanonicalProcessEventAppend;
    pub use crate::runtime::CanonicalRuntimeEffectEnvelope;
    pub use crate::runtime::DirectCompletionClient;
    pub use crate::runtime::EmbeddedRuntimeHost;
    pub use crate::runtime::EventSink;
    pub use crate::runtime::InMemoryLiveReplayStore;
    pub use crate::runtime::InMemoryLiveReplayStoreConfig;
    pub use crate::runtime::LashRuntime;
    pub use crate::runtime::LiveReplayGap;
    pub use crate::runtime::NoopTurnActivitySink;
    pub use crate::runtime::ObservedProcess;
    pub use crate::runtime::ObservedProcessEvent;
    pub use crate::runtime::ObservedProcessEventLite;
    pub use crate::runtime::ObservedProcessEventPage;
    pub use crate::runtime::ObservedProcessEventReadOutcome;
    pub use crate::runtime::ObservedWorkItem;
    pub use crate::runtime::ObservedWorkItemState;
    pub use crate::runtime::ProcessEngineRegistry;
    pub use crate::runtime::ProcessEventAppendPlan;
    pub use crate::runtime::ProcessEventSink;
    pub use crate::runtime::ProcessRuntimeHost;
    pub use crate::runtime::ProcessStartPlan;
    pub use crate::runtime::ProcessToolVisibilityFilter;
    pub use crate::runtime::ProcessTransition;
    pub use crate::runtime::ProcessTransitionPlan;
    pub use crate::runtime::ProcessTurnCancellation;
    pub use crate::runtime::ProcessWorkObserver;
    pub use crate::runtime::ProcessWorkSnapshot;
    pub use crate::runtime::QueuedDrainCandidate;
    pub use crate::runtime::QueuedDrainFamily;
    pub use crate::runtime::QueuedDrainPolicy;
    pub use crate::runtime::QueuedDrainRequest;
    pub use crate::runtime::QueuedDrainSelection;
    pub use crate::runtime::QueuedWorkAuthority;
    pub use crate::runtime::QueuedWorkBatchingConfig;
    pub use crate::runtime::{ObservedProcessChange, ProcessRosterPage};

    pub use crate::runtime::DataRetentionConfig;
    pub use crate::runtime::RuntimeEffectReplayTrace;
    pub use crate::runtime::RuntimeEnvironment;
    pub use crate::runtime::RuntimeEnvironmentBuilder;
    pub use crate::runtime::RuntimeHandle;
    pub use crate::runtime::RuntimeHostConfig;
    pub use crate::runtime::RuntimeObservation;
    pub use crate::runtime::RuntimeSleepOptions;
    pub use crate::runtime::SessionCommand;
    pub use crate::runtime::SessionCommandReceipt;
    pub use crate::runtime::SessionObservation;
    pub use crate::runtime::SessionObservationSubscription;
    pub use crate::runtime::SessionResume;
    pub use crate::runtime::SessionScopeId;
    pub use crate::runtime::load_durable_observation_head;

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

    pub use crate::runtime::process_child_session_id;
    pub use crate::runtime::registry_transitions;
    pub use crate::runtime::terminal_append_request;
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
    pub use crate::session_model::SessionSpec;
    pub use crate::session_model::SpecResolveError;
    pub use crate::session_model::context::PreparedContext;
    pub use crate::store::{CommitBudget, CommitBudgetLimit};
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use crate::tool_registry::PLUGIN_TOOL_SOURCE_ID;
    pub use crate::tool_registry::ReconfigureError;
    pub use crate::tool_registry::SupersededToolIdentity;
    pub use crate::tool_registry::ToolRestoreReport;
    pub use crate::tool_registry::ToolSourceHandle;
    pub use crate::tool_registry::ToolSourcePolicy;
    pub use crate::tool_registry::ToolStateEntry;
    pub use crate::tool_registry::facade_ops::ToolRegistryFacadeOps;
    pub use lash_core_execution::Response;
    pub use lash_core_store::session_graph::facade_ops::{
        SessionGraphFacadeOps, SessionNodeProjection,
    };
    pub use lash_core_store::session_identity::facade_ops::AgentFrameReasonFacadeOps;
    pub use lash_core_store::session_state::facade_ops::RuntimeSessionStateFacadeOps;
    pub use lash_core_store::tool_state::facade_ops::ToolStateFacadeOps;
    pub use lash_core_store::tool_state::{
        ToolMembershipUpdate, ToolStateChange, ToolStateChangeOutcome,
    };
    pub use lash_sansio::AcceptedInjectedTurnInput;
    pub use lash_sansio::AttachmentMaterializationNotice;
    pub use lash_sansio::AttachmentMaterializationReason;
    pub use lash_sansio::AttachmentRef;
    pub use lash_sansio::EffectId;
    pub use lash_sansio::ErrorEnvelope;
    pub use lash_sansio::MessageSequence;
    pub use lash_sansio::ModelToolReturn;
    pub use lash_sansio::ModelToolReturnPart;
    pub use lash_sansio::ProviderSchemaCapabilities;
    pub use lash_sansio::ReportedFailure;
    pub use lash_sansio::ResolvedSchema;
    pub use lash_sansio::RetryProgress;
    pub use lash_sansio::SchemaPurpose;
    pub use lash_sansio::SchemaResolutionError;
    pub use lash_sansio::SchemaResolutionRequest;
    pub use lash_sansio::SessionStreamEvent;
    pub use lash_sansio::StreamMessageKind;
    pub use lash_sansio::ToolCatalogBuildError;
    pub use lash_sansio::TurnFinish;
    pub use lash_sansio::TurnOutcome;
    pub use lash_sansio::TurnStop;
    pub use lash_sansio::append_assistant_text_part;
    pub use lash_sansio::build_tool_catalog;
    pub use lash_sansio::head_tail_truncate;
    pub use lash_sansio::normalized_response_parts;
    pub use lash_sansio::reasoning_part;
    pub use lash_sansio::resolve_schema;
    pub use lash_sansio::shared_parts;
    pub use lash_sansio::tool_result_text;
    pub use lash_sansio::visible_response_text_from_parts;
    pub use lash_trace::JsonlTraceReadError;
    pub use lash_trace::JsonlTraceSink;
    pub use lash_trace::TraceBranchSelection;
    pub use lash_trace::TraceLevel;
    pub use lash_trace::TraceRecord;
    pub use lash_trace::TraceRuntimeScope;
    pub use lash_trace::TraceRuntimeSubject;
    pub use lash_trace::TraceSink;
    pub use lash_trace::TraceSinkError;
    pub use lash_trace::parse_jsonl_records;
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
        ExecutionEnvironmentSyncFailureKind, ExpandedRow, ExpandedWrapper, LlmCallError,
        ModelToolCalls, PendingToolCall, PendingWork, ProtocolDriverHandle, ResponseToolCalls,
        SyncedEnvironment, ToolExpansionPlan, TurnMachine, place_prompt,
    };
}

pub use attachments::{
    AttachmentGcFence, AttachmentPolicy, AttachmentReadPolicy, AttachmentReclamationPolicy,
    AttachmentRootSet, AttachmentStore, AttachmentStoreError, AttachmentStoreFailureClass,
    AttachmentStorePersistence, EmptyRootSetPolicy, StoredAttachment, StoredBlobRef,
};
pub use lash_core_execution::turn_outcome_from_tool_control;
pub use lash_sansio::llm::attachment_delivery::ProviderFileScope;
pub use lash_sansio::llm::types::{
    AttemptOutcome, AttemptRecord, AttemptUsageOutcome, ChargeSafetyDecision,
    ChargeSafetyDenialReason, ExecutionEvidence, ExecutionEvidenceCollectionInterruption,
    ExecutionEvidenceMergeError, GenerationOptionOutcome, GenerationOptions, GenerationReceipt,
    LlmCallId, LlmCallRecord, LlmOutputPart, LlmRequest, LlmRequestOwner, LlmRequestScope,
    LlmResponse, LlmStreamEvidence, LlmTerminalReason, LlmTurnScope, NonNegativeFiniteF64,
    NormalizedError, ProtocolPosition, ProviderEndpointError, ProviderReplayDrop,
    ProviderReplayDropReason, ProviderReplayKind, ProviderRouteIdentity, RecordedRequestTemplate,
    ResponseContext, ResponseContract, RetryClass, RetryDecision, RetryDeclineCause, RetryWait,
    ToolCallContract,
};
pub use lash_sansio::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, Backoff, BatchId,
    BindingChanges, BoundedRetry, CancelOrigin, CancelRequest, CellDefect, CellFailure,
    CellFailureKind, CellOutcome, CellPrint, CellRecord, CheckpointDelivery, CheckpointKind,
    CompactToolContract, DeclarationRefusal, DegradedBinding, ExecCodeFailure,
    ExecCodeFailureReason, ExecResponse, ExecutedCall, ExecutedCallOutcome, ExecutionBudgets,
    ExecutionBudgetsConfig, ExecutionBudgetsError, ExecutionLimit, ExecutionPolicy, FrameKey,
    FrameKeyError, InputId, InternalPartKind, JsonSchema, LimitCause, LlmCallError, LlmUsage,
    MediaType, Message, MessageOrigin, MessageRole, NodeId, OmittedToolCalls, OutcomeShape,
    OutputRetentionPolicy, OutputValue, ParkBound, Part, PartKind, PluginMessage,
    PluginRuntimeEvent, ProjectionMode, ProviderAttemptLimits, RegistrationRefused, RetainedOutput,
    RunId, SchemaAdmissionError, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SessionAppendNode, TOOL_BINDING_KEY, TextProjectionMetadata,
    TokenUsageOverflow, ToolAdmissionRefusal, ToolArgumentProjectionPolicy, ToolBinding, ToolBound,
    ToolBounds, ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolCancellation, ToolCatalog,
    ToolCatalogBuildError, ToolCatalogEntry, ToolCheckConflict, ToolCheckPhase, ToolCheckReply,
    ToolCheckVerdictKind, ToolContract, ToolControl, ToolDeclaration, ToolDefinition,
    ToolDefinitionBindingExt, ToolDiscovery, ToolFailure, ToolFailureCause, ToolFailureClass,
    ToolFailureSource, ToolId, ToolIntentIdentity, ToolIntentKind, ToolManifest, ToolModule,
    ToolOutputContract, ToolValue, ToolView, ToolViewBlock, ToolViewMeta, TurnId, TurnOutputSource,
    TurnReply, ValueMismatch,
};
pub(crate) use lash_sansio::{
    BaseRenderCache, build_turn, messages_are_prompt_resume_safe, visible_response_parts,
};
pub use protocol_build::ProtocolBuildInput;
pub use tool_provider::{ToolAttachmentClient, ToolDirectCompletionClient, ToolSessionLlmProfile};
pub use tool_registry::{
    SupersededToolIdentity, ToolRegistry, ToolRestoreReport, ToolSourcePolicy, ToolState,
};
pub use tool_result::{CancelHint, PendingCompletion, PendingResolver, ToolOutcome};
pub use tool_result::{DeclaredStart, DeclaredStartRefused};

pub(crate) mod facade_ops {}
pub use lash_core_execution::{
    ArtifactCarry, ArtifactCleanup, ArtifactName, ArtifactReferrer, ArtifactReferrerError,
    ArtifactReferrerKind, ArtifactStoreId, AttachmentUploadId, FrameEnvironmentId, HostArtifactPin,
    ReferrerClaim, ReferrerGuard, ReferrerStore, ResolvedArtifactCleanup, RuntimeOwner,
    UploadReferrerId, artifact_referrer_ended,
};
pub use lash_core_execution::{
    ArtifactStoreError, Backend, BackendParts, DurabilityTier, DurableBuildError, DurableConfig,
    DurableSettings, DurableStore, ModuleArtifactAstRefusal, ModuleArtifactCorruption,
    ModuleArtifactGeneration, ModuleArtifactRefusal, ModuleArtifactStore, StoreBindingId, StoreSet,
};
pub use lash_core_execution::{
    DriverAction, DriverContextView, Effect, HostTurnProtocol, PreparedTurnMachine,
    ProjectorContext, ProtocolDriverState, SansIoTurnInput, TurnDriverConfig, TurnDriverPreamble,
    TurnMachine, TurnMachineConfig,
};
pub use lash_sansio::{
    BuildNewestWriterFormats, WriterFormats, build_newest_writer_formats, driver_writer_version,
};
pub use lash_sansio::{
    InvalidToolCallId, ToolCallAdmission, ToolCallId, ToolCallPosition, ToolCallRoot,
    ToolCallRootError,
};

pub use lash_sansio::{
    FailureCode, HostNamespace, InvalidNamespace, Namespace, TurnFailureCode, TurnFailureKind,
};
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{OtelOptions, OtelSpanEnricher, OtelTelemetry};
pub use lash_trace::{
    DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier, InvalidTraceLinks,
    TelemetryContent, TraceAdmissionCandidate, TraceAnchor, TraceAttemptId, TraceCandidateOutcome,
    TraceCarrier, TraceCause, TraceHostOperation, TraceLinks, TraceScopeAdmission,
    TraceScopeFactory, TraceScopeId, TraceScopeKind, TraceScopeOffer, TraceScopeOwner,
    UntracedScopes, W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState,
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
pub(crate) use plugin::PluginRuntimeDirective;
pub use plugin::{
    AdmittedPluginConfig, CandidateFacts, ConfigCommand, ConfigCommandCatalog,
    ConfigCommandDescriptor, ConfigOwner, ConfigRegistrar, ConfigRegistrationError, ConfigRegistry,
    ConfigSubmitError, ConfigTransaction, ConfigWire, CoreConfigOwner, CoreConfigRefusal,
    CreationConfigError, NoRunOptions, OwnerChange, PluginConfig,
};
pub use plugin::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, AppendSessionNodesOutcome,
    AppendSessionNodesRequest, FormatNamespace, FormatRefusal, FormatVersion, FrameNodeId,
    FrameNodeIdError, KeyRejection, PluginConfigNamespace, PluginError, PluginErrorClass,
    PluginExtensions, PluginNamespaceState, PluginOptions, PluginState, PluginStateEffect,
    PluginStateError, PluginStateView, PluginTransitionBase, PluginTransitionId,
    PluginTransitionRecord, PluginTransitionRequest, ProcessEngineContributionContext,
    ProtocolBeforeLlmCallContext, ProtocolLlmCallAction, SessionCreateRequest, SessionGraphService,
    SessionLineage, SessionReadView, SessionRelation, SessionSnapshot, SessionStartPoint,
    SessionStateService, SessionToolAccess, SessionToolAccessError, StateCommands,
    UnstatedSessionConfig, durable_identity_conflict, is_durable_identity_conflict,
};
pub use plugin::{OpenAgentFrameOutcome, OpenAgentFrameRequest};

pub use lash_core_execution::{Material, NamesMaterial, SettledOutput, SettledOutputRefusal};
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
pub use runtime::ConformanceProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use runtime::EffectSummaryAppendFaults;
#[cfg(any(test, feature = "testing"))]
pub use runtime::ProcessEventLogTestSupport;
#[cfg(any(test, feature = "testing"))]
pub use runtime::ProcessRegistryTestSupport;
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestProcessRegistryWriteExt;
#[cfg(any(test, feature = "testing"))]
pub use runtime::{ObservationSource, work_with_observations};

// This block includes the effect / process-control types consumed by host
// process engines and their integration tests —
// they are deliberately public; the rest of the runtime module stays
// crate-internal.
/// A host publishes the execution environment a start names, under the
/// referrer that holds it (FIG-3116, ADR 0113 §3.4).
pub use lash_core_execution::runtime::publish_process_execution_env;
/// The artifact ports a process-engine registry acquires start and revision
/// artifacts through (ADR 0113 §3.3), for hosts that assemble a registry
/// outside `RuntimeHostConfig`.
pub use lash_core_execution::runtime::{ArtifactReferrerPorts, ReferrerAcquisition};
pub use runtime::{
    AbandonEvidence, AbandonWriter, ActiveTurnIngress, AdmittedProcessIdentity, AdmittedScope,
    AdmittedTurnInputs, Ancestry, ArgsMismatch, ArgsMode, AssistantResponseHookEvents,
    AssistantResponsePlan, AssistantStreamHookState, AwaitEventKey, AwaitEventWaitIdentity,
    BindingId, CapabilityRef, CausalRef, ChargeSafetyRefusalEvidence, CheckpointAdmittedSet, Clock,
    ClockWallTime, CommandJournalGuard, CommandReplayKey, ContractRef, DeclaredProcessIdentity,
    DefinitionAcquisition, DefinitionRef, DeliveryPolicy, DeploymentStore,
    DeploymentStoreDecorator, DrainMode, DrainModePolicy, DurableProcessWork, EffectAddress,
    EffectJournalRetirement, EffectOpener, EffectOpenerError, EffectRetirementGate, EngineAction,
    EngineEvent, EngineState, EngineStateFormat, EngineStepKind, EngineStepRefusal, EngineStepRun,
    EngineSteps, ExecutableGeneration, ExecutableGenerationRefusal, ExecutionScope,
    ForkSessionReceipt, ForkSessionRequest, HandleId, InputItem, InspectedProcessDefinition,
    InvalidProcessDefinitionId, InvalidStartKey, JournalReplay, KeyName, Lifetime,
    LifetimeDecision, LifetimePolicy, LiveReplayEventDraft, LiveReplayGapReason, LiveReplayOutcome,
    LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeOutcome, LiveReplaySubscription,
    LlmRequestSpec, LlmStreamRecord, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE,
    MAX_PROCESS_ROSTER_PAGE_SIZE, NoProcessWork, NoRunOptionsOwner, NonTerminalProcessPage,
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION, ParentEndPlan,
    PendingTurnInput, PendingTurnInputBatch, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    PendingTurnInputRead, PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome,
    PreparedProcessRegistration, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeBounds, ProcessChangeCursor, ProcessClockRebind, ProcessCommand,
    ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessDefinition,
    ProcessDefinitionDraft, ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionStore,
    ProcessDefinitionStoredError, ProcessDefinitionTarget, ProcessDefinitionValue, ProcessDocument,
    ProcessDocumentProvider, ProcessDocumentRead, ProcessDocumentRefRead, ProcessDriveStep,
    ProcessEffectNodeReport, ProcessEffectOccurrence, ProcessEffectOmissions,
    ProcessEffectOmittedCounts, ProcessEffectOutcome, ProcessEffectOutcomeClass,
    ProcessEffectReport, ProcessEffectReportError, ProcessEngine, ProcessEngineAdmission,
    ProcessEngineKind, ProcessEngineRegistration, ProcessEngineRegistry, ProcessEvent,
    ProcessEventAppendReceipt, ProcessEventAppendRequest, ProcessEventHistoryRetention,
    ProcessEventKind, ProcessEventLite, ProcessEventLog, ProcessEventPage, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventRelease,
    ProcessExecutionContext, ProcessExecutionDocumentRead, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleView, ProcessId, ProcessIdMint, ProcessIdentity,
    ProcessInfraError, ProcessInput, ProcessLifecycle, ProcessLifecycleFact, ProcessLineage,
    ProcessListFilter, ProcessListMode, ProcessListSelection, ProcessLiveReferenceView,
    ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope, ProcessOriginator,
    ProcessOriginatorFilter, ProcessOutcome, ProcessOutcomeObserver, ProcessParkReason,
    ProcessParkState, ProcessProvenance, ProcessPruneReport, ProcessQuery, ProcessRecord,
    ProcessRegistrar, ProcessRegistration, ProcessRegistrationOutcome, ProcessRegistrationReceipt,
    ProcessRegistry, ProcessRegistryAwaiter, ProcessRegistryCursor, ProcessResumeRefusal,
    ProcessRetention, ProcessRosterCursor, ProcessRosterPage, ProcessRosterRecords,
    ProcessRunOutcome, ProcessService, ProcessSessionDeleteReport, ProcessSignature,
    ProcessSpawnProvenance, ProcessStartDeclaration, ProcessStartOptions, ProcessStartOutcome,
    ProcessStartReceipt, ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTerminalWait, ProcessTombstone, ProcessToolIntents, ProcessWaits, ProcessWorkSubstrate,
    ProcessWorkWiring, ProjectionWatermark, ProtocolSessionExtension, QueuedDrainCandidate,
    QueuedDrainFamily, QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection,
    QueuedWorkAuthority, QueuedWorkBatchingConfig, RecordedKeyFence, RecordedKeyRange,
    RecordedKeys, RecordedRefusal, RecordedRender, RefusedWriteRange, RenderFault, RenderRefusal,
    Resolution, ResolveOutcome, ResolvedProcessDefinition, ResolvedRun, RetainedRevision,
    Retention, RunAggregateWakePolicy, RunDefinition, RunDefinitionRefusal, RunDefinitions,
    RunOptionsOwner, RunOverrides, RunResolveError, RunShapeRefusal, RunSpec, RunSpecHash,
    RuntimeAttribution, RuntimeCheckpointComponents, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchReport, RuntimeError, RuntimeErrorCause, RuntimeErrorCode,
    RuntimeInvocation, RuntimeReplay, RuntimeReplayAttribution, RuntimeSessionAuthority,
    RuntimeSessionState, SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId, ScopeRef,
    ScopeStorageError, SegmentProgress, ServedOnly, ServedOnlyRange, SessionAdministration,
    SessionCreationHead, SessionCursor, SessionCursorError, SessionDeleteContext,
    SessionDeleteExecution, SessionEntry, SessionId, SessionListFilter, SessionObservationEvent,
    SessionObservationEventPayload, SessionProcessEventKind, SessionQueueEventKind,
    SessionRelationKind, SessionRevision, SessionScope, SessionStateVersionRefusal,
    SessionStoreCreateRequest, SessionView, SleepSpec, SlotId, StagedProcessStart, StartCx,
    StartCxError, StartKey, StepName, StepRequest, StoreRealization, StoredDataCorruption, Target,
    ToolAttemptLaunch, TurnActivity, TurnActivityId, TurnCancelAffectedInput,
    TurnCancelInputOutcome, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnCancelWait,
    TurnContext, TurnEvent, TurnFailureCause, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement, TurnInput, TurnInputAdmissionMode, TurnInputApplication,
    TurnInputCheckpointBoundary, TurnInputCompletion, TurnInputCompletionData, TurnInputIngress,
    TurnInputState, TurnInputStateKind, TurnLaneAdmissionPolicy, TurnPrelude, TurnPreludeRef,
    TurnPreludeStore, WaitKind, WaitState, WatchedRegistry, WeakProcessEngineRegistry,
    WorkCadenceError, WorkCadencePolicy, admit_session_state_generation,
    artifact_store_plugin_error, lifetime, mint_process_id, tool_failure_code,
};
#[allow(unused_imports)]
pub(crate) use runtime::{
    AdmissionBoundary, AdmittedQueuedWork, QueuedCheckpointTurnInput, QueuedWorkBatch,
    QueuedWorkBatchDraft, QueuedWorkCompletion, QueuedWorkEnqueueOutcome, QueuedWorkPayload,
    RuntimeSubject, load_process_execution_env, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_event_invocation,
};
pub use runtime::{ConsumerHold, SessionTurnOutcome};
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
    ProcessSequence, RetainedProcessView, StepBodyStartedObservation, commits_bridge,
};
pub use runtime::{ProcessLifecycleState, ProcessOutcomeNotRetained, ProcessTerminal};
pub use runtime::{
    ProcessStartRegistration, ProcessStartTarget, RetiredProcessStatus, TerminalProcessStatus,
};
pub(crate) use session_model::plugin_runtime_protocol_event;

pub(crate) use session::RuntimeExecutionTracing;
pub(crate) use session::Session;
pub use session::{
    ExecRequest, ExecutionEnvironmentSyncError, RuntimeExecutionContext, SessionError,
    ToolDispatchSurface, ToolSurfaceDrift, ToolSurfaceDriftKind, tool_dispatch_surface,
};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SESSION_NODE_BODY_SCHEMA_VERSION, SessionGraph,
    SessionNodePayload, SessionNodeRecord, UndeliveredConfigChange,
};

pub use session_model::LlmProfileBinding;
pub use session_model::{
    ChargeSafetyPolicy, MaxToolCalls, NoProgressBudget, SessionPolicy, ToolCallLimitExceeded,
    ToolCallLimitScope, TurnBudget,
};
pub use session_model::{ProtocolEvent, SessionHistoryRecord};
pub use store::{
    AdoptedAttachmentCondemnation, AppendRequestIdentity, AttachmentCondemnation,
    AttachmentCondemnationAdoption, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentDeleteArming,
    AttachmentDeleteStallReason, AttachmentReferrers, AttachmentSettlementOutcome,
    AttachmentSweepGeneration, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit,
    AttachmentWriteToken, BlobRef, CURRENT_SESSION_STATE_VERSION, CheckpointComponentDescriptor,
    CommitBudget, CommitBudgetLimit, DurableItem, DurablePayload, DurableScan, DurableScanPage,
    DurableSurface, FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState, FleetFormatStore,
    GcReport, HydratedCheckpointComponent, HydratedSessionCheckpoint, LeaseIncarnationId,
    LeaseOwnerId, LeaseOwnerIdentity, MAX_ATTACHMENT_DELETE_ATTEMPTS, MaintenanceFailure,
    MaintenanceRefusal, MaintenanceReport, MaintenanceResult, MaintenanceStop, MaintenanceSweep,
    OLDEST_SUPPORTED_SESSION_STATE_VERSION, OperationId, QueuedWorkStore, RetentionBound,
    RetentionReport, RuntimeCommit, RuntimeStore, RuntimeStoreDecorator, RuntimeTurnCommitStamp,
    ScanCoverage, SemanticBoundaryOperation, SessionAdmission, SessionBlobReclaimReport,
    SessionCatalogStore, SessionCommitStore, SessionHistoryStore, SessionLookup, SessionMeta,
    SessionReferrerState, SessionStateAdmission, SessionStore, StoreBackend, StoreComponentVersion,
    StoreError, StoreMaintenance, StorePreflight, StoreReleaseStamp, StoreReleaseState,
    StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict, SurfaceFormat,
    TurnInputAdmission, TurnInputStore, VacuumReport, WriterPin, compare_releases,
    release_stamp_advances,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphAppend, RuntimeCommitReceipt, SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
    ensure_supported_schema_version,
};
pub use tool_intent::{
    CancelProcessIntent, DeclaredModuleArtifact, GetDefinitionIntent, PublishDefinitionIntent,
    StartProcessIntent, TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT,
    TOOL_INTENT_MAX_PER_KIND, TOOL_INTENT_PROTOCOL_V3, ToolAttemptOutcome, ToolIntent,
    ToolIntentSubmissionAdmission, ToolIntentSubmissionOutcome, ToolIntentSubmissionRecord,
    ToolIntentSubmissionSettlement, ToolIntents, ToolOutcomeDone, derive_tool_intent_identity,
    derive_tool_intent_identity_under, rederive_tool_intent_identity,
};
/// Tool-provider contracts, including child-process execution observation hooks.
pub use tool_provider::{
    AttemptContext, AttemptProcessReads, AttemptSessionReads, IsolatedProcessBinding,
    IsolatedProcessRequest, PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall, ToolCall,
    ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolExecutionGrant, ToolPrepareCall,
    ToolPrepareContext, ToolProvider,
};
#[doc(hidden)]
pub mod core_internal {
    pub use crate::runtime::{
        ProcessRuntimeContext, ProcessRuntimePorts, ProcessStepTools, RuntimeSessionServices,
    };
    pub use lash_core_execution::core_internal::{
        RuntimeEffectLocalRunner, RuntimeExecutionContextRuntimeOps, StartKeyDerivation,
        attach_process_invocation_correlation, clear_process_invocation_correlation,
        owned_runner_executor,
    };
}

pub use lash_core_execution::{
    AttachmentContentMismatch, AttachmentRetentionFailure, AttachmentRetentionStoreFailure,
    CompletedToolCall, Response, ToolIntentCommandFailure, ToolIntentExecutionOutcome,
    ToolIntentRealized, ToolIntentRefusalReason, ToolIntentRuntimeFailure,
};

pub use lash_core_execution::{EffectAttempt, RecordedEffectExecution};

#[cfg(test)]
mod attachments_tests;

pub use lash_core_store::transcript;
