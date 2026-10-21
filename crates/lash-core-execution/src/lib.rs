//! Execution kernel for Lash: process, tool, plugin, session, and effect
//! execution extracted below the `lash-core` orchestration layer.
//!
//! The process kernel understands `SessionTurn` to coordinate child-session
//! turns. Executable process bodies, including work a host runs outside lash,
//! use `ProcessInput::Engine { kind, payload, launch }` and call ordinary recorded tool
//! attempts under their process journal.
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
/// Re-exported so every effect implementation can spell
/// `await_next_settlement`'s cancellation parameter without taking a direct
/// `tokio-util` dependency of its own (FIG-2266).
pub use tokio_util::sync::CancellationToken;

pub use lash_core_store::admitted_scope::wire as admitted_scope_wire;
pub use lash_core_store::artifact_referrer::{
    ArtifactCarry, ArtifactCleanup, ArtifactName, ArtifactReferrer, ArtifactReferrerError,
    ArtifactReferrerKind, ArtifactStoreId, AttachmentUploadId, FrameEnvironmentId, HostArtifactPin,
    ReferrerClaim, ReferrerGuard, ReferrerStore, ResolvedArtifactCleanup,
    SYNTHETIC_NEXT_REFERRER_KIND, UploadReferrerId,
};
pub use lash_core_store::attachments;
pub use lash_core_store::chronological;
pub use lash_core_store::compat;
pub use lash_core_store::config_transaction::{
    CORE_CONFIG_OWNER, ConfigCommandEntry, ConfigFault, ConfigRefusal, ConfigRefusalReason,
    ConfigResolution, ConfigResolutionDecision, ConfigTransactionOutcome, ConfigTransactionRecord,
    ConfigValueRole, CoreConfig, RecordedNamespaceCorrupt, RefusalSite,
};
pub use lash_core_store::impl_current_fleet_format;
pub use lash_core_store::impl_noop_attachment_referrers;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_store::process_id_for_test;
pub use lash_core_store::process_identity::process_env_ref_for_bytes;
pub use lash_core_store::process_identity::process_id_from_handle_json;
pub use lash_core_store::protocol_turn_options::{ProtocolTurnOptions, ProtocolTurnOptionsError};
pub use lash_core_store::runtime_error::{
    AttachmentContentMismatch, AttachmentRetentionFailure, AttachmentRetentionStoreFailure,
    IngressReservedSourceKeyRefusal,
};
pub use lash_core_store::runtime_owner::RuntimeOwner;
pub use lash_core_store::session_identity::SessionRevision;
pub use lash_core_store::surface_format;

/// The durable store a [`StoreSet`] hands out (ADR 0132 §1).
pub use lash_durable::{DurableConfig, DurableSettings, DurableStore};
pub use lash_protocol_types::{
    module_artifact_refusal::ModuleArtifactCorruption,
    module_artifact_refusal::ModuleArtifactRefusal,
};

pub mod direct;
pub mod direct_completion_client;
pub mod effect_attempt;
pub use effect_attempt::{EffectAttempt, RecordedEffectExecution};
pub mod engine;

pub(crate) use lash_core_ids::operational_metrics;
pub use lash_core_llm::llm;
pub(crate) use lash_core_llm::llm_profile;
/// Panic containment for runtime-owned work.
///
/// The module lives in `lash-core-ids`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helpers crate-internal.
pub mod panic_containment {
    pub(crate) use lash_core_ids::panic_containment::payload_message;
    pub use lash_core_ids::panic_containment::{is_loud, set_loud};
}
#[cfg(feature = "perf-witness")]
pub use lash_core_ids::perf_witness;
pub mod plugin;
pub mod plugin_stack;
pub mod protocol_build;
/// Provider components for pluggable LLM backends.
///
/// The module lives in `lash-core-llm`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helper crate-internal.
pub mod provider {
    pub use lash_core_llm::provider::*;
}
pub mod runtime;
pub mod session;
pub use lash_core_store::prompt_sections;
pub use lash_core_store::session_graph;
pub mod session_model;
/// Stable hashing primitives, re-exported from `lash-core-ids`. The helpers
/// stay crate-internal; the module itself is public under `testing` exactly as
/// it was before the carve-out.
#[cfg(feature = "testing")]
pub mod stable_hash {
    pub(crate) use lash_core_ids::stable_hash::stable_json_string;
    pub use lash_core_ids::stable_hash::{blake3_hex, sha256_hex};
}
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_ids::stable_hash;
pub(crate) use lash_core_ids::stable_identity;
pub mod store;
pub use lash_core_ids::task;
pub use lash_core_store::store_backend_support;
pub use lash_core_store::tool_run;
/// Standard-lock poison recovery traits used across Lash hosts and runtimes.
pub mod sync {
    pub use lash_protocol_types::sync::*;
}
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_ids::test_watchdog;
pub mod deferred_provider;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tool_dispatch;

pub mod tool_provider;
pub mod tool_reads;
pub mod tool_registry;
pub mod tool_result;
pub mod trace;

mod finish_tool;
pub use finish_tool::{FinishToolProvider, finish_tool, finish_tool_id, suppress_default_finish};

pub mod facade_support {
    pub use crate::Response;
    pub use crate::runtime::effect::scope_status;
    pub use lash_core_ids::operational_metrics::{DurableCommitCost, StoreObserver, sql};
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

    pub use crate::plugin::AfterToolContributions;
    pub use crate::plugin::AfterToolDecision;
    pub use crate::plugin::BeforeToolDecision;
    pub use crate::plugin::CachedToolSuccess;
    pub use crate::plugin::HookKey;
    pub use crate::plugin::PluginAbort;
    pub use crate::plugin::PluginRecordContribution;
    pub use crate::plugin::PluginStateView;
    pub use crate::plugin::PreparedCallReadView;
    pub use crate::plugin::SessionLifecycleService;
    pub use crate::plugin::SessionParam;
    pub use crate::plugin::SessionPlugin;
    pub use crate::plugin::ToolArgsCheckInput;
    pub use crate::plugin::ToolArgsTransformInput;
    pub use crate::plugin::ToolCatalogContribution;
    pub use crate::plugin::ToolHookContext;
    pub use crate::plugin::ToolPresentationArtifacts;
    pub use crate::plugin::ToolPresentationInput;
    pub use crate::plugin::ToolPresentationPresenter;
    pub use crate::plugin::ToolPresentationStep;
    pub use crate::plugin::ToolResultCandidate;
    pub use crate::plugin::ToolResultCheckInput;
    pub use crate::plugin::ToolResultProjectionContext;
    pub use crate::plugin::ToolResultTransformInput;
    pub use crate::plugin::TurnContributions;
    pub use crate::plugin::TurnHookContext;
    pub use crate::plugin::TurnHookReport;
    pub use crate::plugin::TurnResultHookContext;
    pub use lash_core_store::plugin_state::KeyRejection;
    pub use lash_core_store::plugin_state::PluginStateError;
    pub use lash_core_store::session_identity::SessionObserverIntent;
    pub use lash_core_store::tool_run::HookCause;
    pub use lash_core_store::tool_run::StateCommand;
    pub use lash_core_store::tool_run::StateCommandRefusal;
    pub use lash_core_store::tool_run::ToolHookOccurrence;

    pub use crate::plugin::StateReducer;
    pub use crate::plugin::StateReduction;
    pub use crate::plugin::{ModelCallAddress, SessionConfigChangedContext, SessionConfigField};
    pub use crate::plugin::{
        PluginFailureClass, PluginFailureOrigin, PluginHookFailure, PluginOperationFailure,
    };
    pub use crate::plugin_stack::PluginStack;
    pub use lash_core_llm::provider::CacheRetention;
    pub use lash_core_llm::provider::GenerationRetryGuarantee;
    pub use lash_core_llm::provider::LlmProfileEffortValidationCategory;
    pub use lash_core_llm::provider::LlmTimeouts;
    pub use lash_core_llm::provider::Provider;
    pub use lash_core_llm::provider::ProviderComponents;
    pub use lash_core_llm::provider::ProviderHandle;
    pub use lash_core_llm::provider::ProviderOptions;

    pub use crate::runtime::CanonicalRuntimeEffectEnvelope;
    pub use crate::runtime::DataRetentionConfig;
    pub use crate::runtime::DirectCompletionClient;
    pub use crate::runtime::EmbeddedRuntimeHost;
    pub use crate::runtime::EventSink;
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
    pub use crate::runtime::RuntimeEffectReplayTrace;
    pub use crate::runtime::RuntimeSleepOptions;
    pub use crate::runtime::SessionCommand;
    pub use crate::runtime::SessionCommandReceipt;

    pub use crate::runtime::{RuntimeDataLimits, RuntimeHostConfig};

    pub use crate::runtime::TurnActivitySink;
    pub use crate::runtime::TurnAttach;
    pub use crate::runtime::TurnCancelMode;
    pub use crate::runtime::TurnCancelOutcome;
    pub use crate::runtime::TurnCancelReceipt;
    pub use crate::runtime::TurnCancelUndeliveredInputPolicy;
    pub use crate::runtime::TurnCancellationEvidence;
    pub use crate::runtime::TurnExecutionMetrics;
    pub use crate::runtime::TurnIssue;
    pub use crate::runtime::TurnIssueSeverity;
    pub use crate::runtime::TurnLaneAdmissionPolicy;
    pub use crate::runtime::TurnTerminal;
    pub use crate::runtime::TurnWorkDriver;
    pub use lash_core_ids::clock::SystemClock;
    pub use lash_core_store::turn_control_vocabulary::TurnAddress;
    pub use lash_core_store::turn_control_vocabulary::TurnCancelAffectedInput;
    pub use lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome;
    pub use lash_core_store::turn_control_vocabulary::TurnCancelRequest;
    pub use lash_core_store::turn_input_vocabulary::TurnInputAcceptanceReceipt;

    pub use crate::runtime::WatchedRegistry;
    pub use crate::runtime::current_epoch_ms;

    pub use crate::runtime::validate_replayed_effect_envelope;
    pub use crate::runtime::watch_process_registry;
    pub use crate::runtime::{ProcessChangeHub, ProcessChangeSubscription};
    pub use crate::session::InjectedTurnInput;
    pub use crate::session::ToolInvocation;
    pub use crate::session::ToolInvocationReply;
    pub use crate::session_graph::frame_node_id;
    pub use crate::session_model::ConversationRecord;
    pub use lash_core_store::session_policy::GenerationOverlay;

    pub use crate::session_model::context::PreparedContext;
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use lash_core_store::session_graph::facade_ops::{
        SessionGraphFacadeOps, SessionNodeProjection,
    };
    pub use lash_core_store::session_identity::facade_ops::AgentFrameReasonFacadeOps;
    pub use lash_core_store::session_state::facade_ops::RuntimeSessionStateFacadeOps;
    pub use lash_core_store::store::CommitBudget;
    pub use lash_core_store::store::CommitBudgetLimit;
    pub use lash_core_store::tool_surface::ReconfigureError;
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
    pub use lash_trace::TraceRecord;
    pub use lash_trace::TraceRuntimeScope;
    pub use lash_trace::TraceRuntimeSubject;
    pub use lash_trace::TraceSink;
    pub use lash_trace::TraceSinkError;
    pub use schemars::JsonSchema;
}

pub(crate) use facade_support::*;

// `facade_support` is the workspace's internal cross-crate seam, and membership
// in it means some crate's *shipped* code needs the item (FIG-1223). These
// twelve had test-only consumers, so their public path is `test_support` and
// only their crate-internal short path lives here: `test_support` is
// feature-gated and `crate::X` has to resolve in every build.
pub(crate) use crate::plugin::RuntimeServices;
pub(crate) use lash_core_store::session_identity::SessionObservedProcessOutcome;
pub(crate) use lash_core_store::session_identity::SessionObservedProcessReceipt;
pub(crate) use lash_core_store::session_identity::SessionObserverIntent;
pub(crate) use lash_protocol_types::{ToolCatalogBuildInput, validate_tool_input};

pub mod sansio {
    pub use crate::{CompletedToolCall, Response};
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
pub use lash_protocol_types::llm::attachment_delivery::ProviderFileScope;
pub use lash_protocol_types::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, Backoff, BatchId,
    BindingChanges, BoundedRetry, CancelOrigin, CancelRequest, CellControl, CellDefect,
    CellFailure, CellFailureKind, CellFailureRepair, CellOutcome, CellPrint, CellRecord,
    CheckpointKind, CompletionCandidate, CompletionDisposition, DecidedCompletion,
    DeclarationRefusal, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason, ExecResponse,
    ExecutedCall, ExecutedCallOutcome, ExecutionBudgets, ExecutionBudgetsConfig,
    ExecutionBudgetsError, ExecutionLimit, ExecutionPolicy, FrameKey, FrameKeyError, InputId,
    IntoOperation, JsonSchema, LimitCause, LlmCallError, LlmUsage, MediaType, Message,
    MessageOrigin, MessageRole, NextRun, NodeId, OutcomeShape, OutputRetentionPolicy, OutputValue,
    ParkBound, Part, PartKind, PluginMessage, PluginRuntimeEvent, ProjectionMode,
    ProviderAttemptLimits, QualifiedOperation, RegistrationRefused, RetainedOutput, RetainedValue,
    SchemaAdmissionError, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SessionAppendNode, TerminationMode, TextProjectionMetadata,
    TokenUsageOverflow, ToolArgumentProjectionPolicy, ToolBound, ToolBounds, ToolCallOutcome,
    ToolCallOutput, ToolCallRecord, ToolCancellation, ToolCatalog, ToolCatalogBuildError,
    ToolCatalogEntry, ToolCheckConflict, ToolCheckPhase, ToolCheckReply, ToolCheckVerdictKind,
    ToolContract, ToolControl, ToolDeclaration, ToolDefinition, ToolDiscovery, ToolDraft,
    ToolFailure, ToolFailureCause, ToolFailureClass, ToolFailureSource, ToolId, ToolIntentIdentity,
    ToolIntentKind, ToolManifest, ToolOutputContract, ToolValue, ToolView, ToolViewBlock,
    ToolViewMeta, TurnControl, TurnControlKind, TurnControls, TurnId, TurnOutputSource,
    ValueMismatch, ValueShape, ValueShapeLimits,
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
    llm::types::LlmTerminalReason, llm::types::NonNegativeFiniteF64, llm::types::NormalizedError,
    llm::types::ProtocolPosition, llm::types::ProviderEndpointError,
    llm::types::ProviderReplayDrop, llm::types::ProviderReplayDropReason,
    llm::types::ProviderReplayKind, llm::types::ProviderRouteIdentity,
    llm::types::RecordedRequestTemplate, llm::types::RetryDecision,
};
pub use lash_sansio::CheckpointDelivery;
pub use tool_provider::{ToolAttachmentClient, ToolDirectCompletionClient, ToolSessionLlmProfile};
/// The stop a tool control other than a declared turn control ends the turn
/// with: a tool-requested failure, or a plugin's abort. A declared turn
/// control ([`ToolControl::Turn`]) is no stop: it records a
/// [`CompletionCandidate`](crate::CompletionCandidate) that BeforeCompletion
/// decides.
///
/// # Integrator class
///
/// Protocol-engine implementors use this shared projection to preserve the
/// host's terminal-outcome semantics.
pub fn turn_stop_from_tool_control(tool_name: &str, control: &ToolControl) -> Option<TurnOutcome> {
    match control {
        ToolControl::Turn { .. } => None,
        ToolControl::Fail { failure } => Some(TurnOutcome::Stopped(TurnStop::ToolError {
            tool_name: tool_name.to_string(),
            value: tool_failure_for_projection(failure),
        })),
        ToolControl::AbortRun { .. } => Some(TurnOutcome::Stopped(TurnStop::PluginAbort)),
    }
}

fn tool_value_for_projection(value: &ToolValue) -> serde_json::Value {
    ToolCallOutput::success_tool_value(value.clone()).value_for_projection()
}

fn tool_failure_for_projection(failure: &ToolFailure) -> serde_json::Value {
    let mut projected = failure.to_json_value();
    if let Some(raw) = failure.raw.as_ref() {
        projected["raw"] = tool_value_for_projection(raw);
    }
    projected
}
pub use deferred_provider::{
    DeferredResolveContext, DeferredToolProvider, DeferredToolProviderRegistration, DispatchGrant,
    DispatchGrantRefusal, DispatchRefusal, DispatchTarget, DispatchTransform, TransformRefusal,
};
pub use lash_core_store::obligations::{ObligationToken, RegistryKind};
pub use lash_core_store::tool_surface::{
    DeferredMatch, DeferredOutcome, DeferredResolutionPolicy, ExecutionBinding,
    ExecutionBindingUnreadable, Judgement, LinkSource, OperationLink, ProviderKey, RecordedSurface,
    SurfaceCollision, SurfaceDuplicateTool, SurfaceEntry, SurfaceRefusal, judge,
};
pub use lash_kernel_doc::EffectName;
pub use protocol_build::{FleetWriterFormats, ProtocolBuildInput};
pub use tool_reads::{ReadCall, ReadError, ReadRoute, ToolProviderRegistration, ToolReads};
pub use tool_registry::{
    SurfaceTools, ToolRegistry, ToolSurfaceDrift, ToolSurfaceDriftKind, operation_contract_changed,
};
pub use tool_result::{CancelHint, PendingCompletion, PendingResolver, ToolOutcome};

pub(crate) mod facade_ops {}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
/// Durable protocol-driver state owned by protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors persist this envelope while the facade owns
/// orchestration and lifecycle policy.
pub struct ProtocolDriverState {
    pub plugin_id: String,
    pub payload: serde_json::Value,
}

impl ProtocolDriverState {
    /// Wraps one plugin's durable driver payload for protocol-engine implementors persisting
    /// turn-machine state across suspension.
    pub fn new(plugin_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostTurnProtocol;

impl lash_sansio::TurnProtocol for HostTurnProtocol {
    type IntentOutcome = ToolIntentExecutionOutcome;
    type Event = crate::session_model::ProtocolEvent;
    type Termination = ProtocolTurnOptions;
    type DriverState = ProtocolDriverState;
}

/// Host-specialized effect vocabulary for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors shift these effects; applications use the facade.
pub type Effect = lash_sansio::Effect<HostTurnProtocol>;
/// Host-specialized driver action for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors return these actions; applications use the facade.
pub type DriverAction = lash_sansio::DriverAction<HostTurnProtocol>;
/// Borrowed host driver context for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors inspect this view while advancing a turn.
pub type DriverContextView<'a> = lash_sansio::DriverContextView<'a, HostTurnProtocol>;
/// Host driver configuration consumed by protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors configure their driver through this type.
pub type TurnDriverConfig = lash_sansio::TurnDriverConfig<HostTurnProtocol>;
/// Host driver preamble produced by protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors use this while preparing a turn.
pub type TurnDriverPreamble = lash_sansio::TurnDriverPreamble<HostTurnProtocol>;
/// Host projector context for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors use this context to project protocol state.
pub type ProjectorContext<'a> = lash_sansio::ProjectorContext<'a, HostTurnProtocol>;
/// Prepared host turn machine handed to protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors complete preparation before executing the machine.
pub type PreparedTurnMachine = lash_sansio::PreparedTurnMachine<HostTurnProtocol>;
/// Host-specialized input for Sans-I/O protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors accept this typed boundary input.
pub type SansIoTurnInput = lash_sansio::SansIoTurnInput<HostTurnProtocol>;
/// Host-specialized state machine for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors shift this machine; applications use the facade.
pub type TurnMachine = lash_sansio::TurnMachine<HostTurnProtocol>;
/// Host turn-machine configuration for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors construct this configuration at their boundary.
pub type TurnMachineConfig = lash_sansio::TurnMachineConfig<HostTurnProtocol>;
pub use lash_protocol_types::{FailureCode, TurnFailureCode, TurnFailureKind};
pub use lash_protocol_types::{
    InvalidToolCallId, ToolCallAdmission, ToolCallId, ToolCallPosition, ToolCallRoot,
    ToolCallRootError,
};

pub use lash_core_store::execution_state::AdmittedPluginConfig;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{
    LashTracerProvider, OtelOptions, OtelTelemetry, SpanEnricher, SpanEnrichment, SpanView,
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
pub use plugin::CandidateFacts;
pub use plugin::ConfigCommand;
pub use plugin::ConfigCommandCatalog;
pub use plugin::ConfigCommandDescriptor;
pub use plugin::ConfigOwner;
pub use plugin::ConfigRegistrar;
pub use plugin::ConfigRegistry;
pub use plugin::ConfigSubmitError;
pub use plugin::ConfigTransaction;
pub use plugin::ConfigWire;
pub use plugin::CoreConfigOwner;

pub use lash_core_store::execution_state::PluginConfig;
pub use plugin::CreationConfigError;
pub use plugin::NoRunOptions;
pub use plugin::OwnerChange;
pub use plugin::OwnerConfigView;

pub use lash_core_store::execution_state::PluginOptions;
pub use lash_core_store::plugin_state::FormatNamespace;
pub use lash_core_store::plugin_state::FormatRefusal;
pub use lash_core_store::plugin_state::KeyRejection;
pub use lash_core_store::plugin_state::PluginConfigNamespace;
pub use lash_core_store::plugin_state::PluginNamespaceState;
pub use lash_core_store::plugin_state::PluginState;
pub use lash_core_store::plugin_state::PluginStateError;
pub use lash_core_store::session_identity::AgentFrameAssignment;
pub use lash_core_store::session_identity::AgentFrameReason;
pub use lash_core_store::session_identity::AgentFrameRecord;
pub use lash_core_store::session_identity::FrameNodeId;
pub use lash_core_store::session_identity::FrameNodeIdError;
pub use lash_core_store::session_identity::SessionLineage;
pub use lash_core_store::session_identity::SessionRelation;
pub use lash_core_store::session_identity::SessionSnapshot;
pub use plugin::AppendSessionNodesOutcome;
pub use plugin::AppendSessionNodesRequest;
pub use plugin::FormatVersion;
pub use plugin::PluginExtensions;
pub use plugin::PluginStateEffect;
pub use plugin::PluginStateView;
pub use plugin::PluginTransitionBase;
pub use plugin::PluginTransitionId;
pub use plugin::PluginTransitionRecord;
pub use plugin::PluginTransitionRequest;
pub use plugin::ProcessEngineContributionContext;
pub use plugin::ProtocolBeforeLlmCallContext;
pub use plugin::ProtocolLlmCallAction;
pub use plugin::SessionGraphService;
pub use plugin::SessionReadView;
pub use plugin::SessionStateService;

pub use lash_core_store::session_identity::OpenAgentFrameOutcome;
pub use lash_core_store::session_identity::OpenAgentFrameRequest;
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

pub(crate) use runtime::default_queued_drain_policy;

// This block includes the effect / process-control types consumed by host
// process engines and their integration tests —
// they are deliberately public; the rest of the runtime module stays
// crate-internal.
pub use runtime::ExecutionOwner;
pub(crate) use runtime::ToolAttemptEffectOutcome;
pub use runtime::TurnCancelWait;
pub use runtime::actor::ActorContext;

pub use runtime::actor::waits;
pub use runtime::actor::waits::ResolveAnswer;

pub use lash_core_ids::clock::Clock;
pub use lash_core_ids::clock::ClockWallTime;
pub use lash_core_store::queued_work_vocabulary::DeliveryPolicy;
pub use lash_core_store::run_spec::BindingId;
pub use lash_core_store::run_spec::CapabilityRef;
pub use lash_core_store::run_spec::ContractRef;
pub use lash_core_store::run_spec::DefinitionRef;
pub use lash_core_store::turn_input_vocabulary::AdmittedTurnInputs;
pub use runtime::AdmittedDirectSend;
pub use runtime::AdmittedScope;
pub use runtime::AssistantResponseHookEvents;
pub use runtime::AssistantResponsePlan;
pub use runtime::AssistantStreamHookState;
pub use runtime::AwaitEventKey;
pub use runtime::AwaitEventWaitIdentity;
pub use runtime::CausalRef;
pub use runtime::ChargeSafetyRefusalEvidence;
pub use runtime::CheckpointAdmittedSet;
pub use runtime::CommandJournalGuard;
pub use runtime::CommandReplayKey;

pub use lash_core_store::effect_opener::EffectOpener;
pub use runtime::DeploymentStoreDecorator;
pub use runtime::DrainMode;
pub use runtime::DrainModePolicy;
pub use runtime::DurableProcessWork;
pub use runtime::EffectAddress;
pub use runtime::EffectJournalRetirement;
pub use runtime::EffectOpenerError;
pub use runtime::EffectRetirementGate;
pub use runtime::EnclosingProcess;
pub use runtime::ExecutableGeneration;
pub use runtime::ExecutableGenerationRefusal;
pub use runtime::ExecutionScope;

pub use lash_core_store::process_identity::InvalidStartKey;
pub use lash_core_store::process_identity::ProcessEnvRef;
pub use lash_core_store::process_identity::ProcessIdMint;
pub use lash_core_store::run_spec::NoRunOptionsOwner;
pub use lash_core_store::run_spec::OwnerRunOptions;
pub use lash_core_store::turn_input_vocabulary::InputItem;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInput;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputBatch;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputCancelOutcome;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputCancelReceipt;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputCancelTarget;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputDraft;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputRead;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputReadStatus;
pub use lash_core_store::turn_input_vocabulary::PendingTurnInputSuffixCancelOutcome;
pub use lash_protocol_types::ProcessId;
pub use runtime::ForkSessionReceipt;
pub use runtime::ForkSessionRequest;
pub use runtime::HandleId;
pub use runtime::HoldEnvError;
pub use runtime::InvalidProcessDefinitionId;
pub use runtime::JournalReplay;
pub use runtime::LlmRequestSpec;
pub use runtime::LlmStreamRecord;
pub use runtime::NoProcessWork;
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
pub use runtime::ProcessEnvRefusal;
pub use runtime::ProcessEnvSelection;
pub use runtime::ProcessLimits;
pub use runtime::ProcessListSelection;
pub use runtime::ProcessModel;
pub use runtime::ProcessOpScope;
pub use runtime::ProcessOutcomeObserver;
pub use runtime::ProcessService;
pub use runtime::ProcessStartChoice;

pub use lash_core_store::effect_identity::RuntimeAttribution;
pub use lash_core_store::effect_identity::RuntimeEffectKind;
pub use lash_core_store::effect_identity::RuntimeInvocation;
pub use lash_core_store::effect_identity::RuntimeReplay;
pub use lash_core_store::effect_identity::RuntimeReplayAttribution;
pub use lash_core_store::process_identity::StartKey;
pub use lash_core_store::run_spec::RecordedRender;
pub use lash_core_store::run_spec::RenderFault;
pub use lash_core_store::run_spec::RenderRefusal;
pub use lash_core_store::run_spec::ResolvedRun;
pub use lash_core_store::run_spec::RunDefinition;
pub use lash_core_store::run_spec::RunDefinitionRefusal;
pub use lash_core_store::run_spec::RunDefinitions;
pub use lash_core_store::run_spec::RunOptionsEncodeError;
pub use lash_core_store::run_spec::RunOptionsKey;
pub use lash_core_store::run_spec::RunOptionsOwner;
pub use lash_core_store::run_spec::RunOverrides;
pub use lash_core_store::run_spec::RunResolveError;
pub use lash_core_store::run_spec::RunShapeRefusal;
pub use lash_core_store::run_spec::RunSpec;
pub use lash_core_store::run_spec::RunSpecHash;
pub use lash_core_store::run_spec::SlotId;
pub use lash_core_store::runtime_error::RuntimeEffectControllerError;
pub use lash_core_store::runtime_error::RuntimeEffectReplayMismatchReport;
pub use lash_core_store::runtime_error::RuntimeError;
pub use lash_core_store::runtime_error::RuntimeErrorCode;
pub use lash_core_store::runtime_error::SessionStateVersionRefusal;
pub use lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION;
pub use lash_core_store::scope_identity::ScopeId;
pub use lash_core_store::scope_identity::ScopeStorageError;
pub use lash_core_store::session_catalog::SessionEntry;
pub use lash_core_store::session_catalog::SessionListFilter;
pub use lash_core_store::session_catalog::SessionRelationKind;
pub use lash_core_store::session_catalog::SessionView;
pub use lash_core_store::session_identity::SessionCreationHead;
pub use lash_core_store::session_identity::SessionStoreCreateRequest;
pub use lash_core_store::session_state::RuntimeCheckpointComponents;
pub use lash_core_store::session_state::RuntimeSessionAuthority;
pub use lash_core_store::session_state::RuntimeSessionState;
pub use lash_core_store::store::RetainedRevision;
pub use lash_core_store::store::Retention;
pub use lash_core_store::store::Target;
pub use lash_core_store::turn_control_vocabulary::TurnCancelAffectedInput;
pub use lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome;
pub use lash_core_store::turn_input_vocabulary::SubmittedBody;
pub use lash_core_store::turn_input_vocabulary::TurnActivityId;
pub use lash_core_store::turn_input_vocabulary::TurnContext;
pub use lash_core_store::turn_input_vocabulary::TurnInput;
pub use lash_core_store::turn_input_vocabulary::TurnInputAdmissionMode;
pub use lash_core_store::turn_input_vocabulary::TurnInputApplication;
pub use lash_core_store::turn_input_vocabulary::TurnInputCheckpointBoundary;
pub use lash_core_store::turn_input_vocabulary::TurnInputCompletion;
pub use lash_core_store::turn_input_vocabulary::TurnInputCompletionData;
pub use lash_core_store::turn_input_vocabulary::TurnInputIngress;
pub use lash_core_store::turn_input_vocabulary::TurnInputState;
pub use lash_protocol_types::SessionId;
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
pub use runtime::RefusedWriteRange;
pub use runtime::Resolution;
pub use runtime::ResolveOutcome;
pub use runtime::ResolvedInlineStart;
pub use runtime::ResolvedStartTarget;
pub use runtime::RunAggregateWakePolicy;
pub use runtime::RuntimeEffectCommand;
pub use runtime::RuntimeEffectEnvelope;
pub use runtime::RuntimeEffectInvocation;
pub use runtime::RuntimeEffectLocalExecutor;
pub use runtime::RuntimeEffectOutcome;
pub use runtime::RuntimeErrorCause;
pub use runtime::SegmentProgress;
pub use runtime::ServedOnly;
pub use runtime::ServedOnlyRange;
pub use runtime::SleepSpec;
pub use runtime::StagedProcessStart;
pub use runtime::StoredDataCorruption;
pub use runtime::ToolAttemptLaunch;
pub use runtime::ToolIntentActivityOutcome;
pub use runtime::ToolIntentRealizedRef;
pub use runtime::TurnActivity;
pub use runtime::TurnCancelMode;
pub use runtime::TurnCancelUndeliveredInputPolicy;
pub use runtime::TurnEvent;
pub use runtime::TurnFailureCause;
pub use runtime::TurnFailureEvidence;
pub use runtime::TurnFailurePartialOutput;
pub use runtime::TurnFailureSettlement;
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

pub(crate) use runtime::QueuedWorkBatch;

pub(crate) use lash_core_store::effect_identity::RuntimeSubject;

pub use runtime::request_process_successor;

pub use session::{
    CellEndHandoff, EndedCell, ExecRequest, ExecutionEnvironmentSyncError, HeldResponse,
    RuntimeExecutionContext, SessionError,
};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SESSION_NODE_BODY_SCHEMA_VERSION, SessionGraph,
    SessionNodePayload, SessionNodeRecord, UndeliveredConfigChange,
};
pub use session_model::LlmProfileBinding;
pub(crate) use session_model::RuntimeSessionPolicy;

pub use lash_core_store::session_policy::SessionPolicy;
pub use session_model::ChargeSafetyPolicy;
pub use session_model::MaxToolCalls;
pub use session_model::NoProgressBudget;
pub use session_model::ToolCallLimitExceeded;
pub use session_model::ToolCallLimitScope;
pub use session_model::TurnBudget;
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

/// The process wiring a process host hands the runtime for a tool call
/// running inside a durable process.
/// Tool-provider contracts, including child-process execution observation hooks.
pub use tool_provider::{
    AttemptContext, AttemptProcessReads, AttemptSessionReads, IsolatedProcessRequest,
    IsolatedStart, PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall, ResolveTargetError,
    ToolCall, ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolExecutionGrant,
    ToolPrepareCall, ToolPrepareContext, ToolProvider,
};

pub(crate) use lash_core_store::process_identity::StartKeyDerivation;
pub(crate) use tool_provider::ToolContext;

#[doc(hidden)]
pub mod core_internal {
    /// Implemented by runtime capability adapters; host substitutes require `testing`.
    pub trait RuntimeService {}

    pub use crate::direct_completion_client::{
        DirectCompletionService, DirectExecutionPosition, runtime_direct_completion_client,
    };
    pub use crate::plugin::EffectPublication;
    pub use crate::runtime::effect::executor::{RuntimeEffectLocalRunner, owned_runner_executor};
    pub use crate::runtime::effect::executor::{sleep_duration, sleep_with_cancellation};
    pub use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps;
    pub use lash_core_store::process_identity::StartKeyDerivation;
    /// The cut a model call's renderers read (ADR 0133 §4): built only by
    /// the runtime, from committed state, at the call's admission.
    pub fn prompt_cut(
        parts: crate::plugin::prompt::PromptCutParts,
        protocol: Option<crate::plugin::prompt::ProtocolPromptFacts>,
    ) -> crate::plugin::prompt::PromptCut {
        crate::plugin::prompt::PromptCut::new(parts).with_protocol_facts(protocol)
    }

    /// Compose a model call's prompt (ADR 0133 §5 to §7): the one route from
    /// a session's sections to a call's text and snapshot, run only at the
    /// call's admission.
    ///
    /// # Errors
    ///
    /// The typed [`PromptCompositionError`](crate::plugin::prompt::PromptCompositionError).
    pub async fn compose_prompt(
        catalog: &crate::plugin::prompt::PromptCatalog,
        plan: &crate::prompt_sections::PromptPlan,
        purpose: &crate::prompt_sections::PromptPurpose,
        cut: std::sync::Arc<crate::plugin::prompt::PromptCut>,
        pool: &crate::plugin::prompt::PromptRenderPool,
    ) -> Result<crate::plugin::prompt::ComposedPrompt, crate::plugin::prompt::PromptCompositionError>
    {
        catalog.compose(plan, purpose, cut, pool).await
    }

    /// Carries the lineage of the process a live child turn runs inside, so
    /// every start made in that turn records it (FIG-3607 R1).
    pub fn attach_process_lineage(
        turn_context: &mut crate::TurnContext,
        lineage: lash_execution_contracts::process::ProcessLineage,
    ) {
        crate::session::attach_process_lineage(turn_context, lineage);
    }
}

mod tool_attempt_outcome;
pub use tool_attempt_outcome::{ToolAttemptOutcome, ToolOutcomeDone};

use lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome;

/// The host protocol's completion, including its typed durable intent outcomes.
pub type CompletedToolCall = lash_sansio::CompletedToolCall<ToolIntentExecutionOutcome>;
/// The host protocol's response to an execution effect.
pub type Response = lash_sansio::Response<ToolIntentExecutionOutcome>;

pub mod tool_live;
pub use tool_live::{
    CallAddress, TOOL_LIVE_ENVELOPE_BYTES, ToolLive, ToolLiveCursor, ToolLiveDiagnostics,
    ToolLiveEmission, ToolLiveEmissionId, ToolLiveError, ToolLiveFact, ToolLiveFaultSource,
    ToolLiveFilter, ToolLiveGapReason, ToolLiveItem, ToolLiveLogLevel, ToolLiveMemoryWindow,
    ToolLiveOmission, ToolLiveOwnerOmission, ToolLivePayload, ToolLivePolicy, ToolLivePolicyBound,
    ToolLivePolicyError, ToolLivePolicyField, ToolLivePolicyRelation, ToolLivePolicySnapshot,
    ToolLivePolicyValue, ToolLivePublisher, ToolLiveReadFault, ToolLiveRecord, ToolLiveReplay,
    ToolLiveRetention, ToolLiveSubscription,
};

/// Explicit internal process serialization for store implementations.
use lash_execution_contracts::process::Durable;

use lash_execution_contracts::*;

/// Host-selected working-copy limits for durable owners.
pub use lash_durable::OwnerRetentionSettings;
