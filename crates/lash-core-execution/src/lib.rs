//! Execution kernel for Lash: process, tool, plugin, session, and effect
//! execution extracted below the `lash-core` orchestration layer.
//!
//! The process kernel intentionally understands `ToolCall`, `SessionTurn`, and
//! `External` because those inputs carry runtime mechanisms core must enforce:
//! tool orchestration, child-session turns, and externally completed work. New
//! process runtimes should use `ProcessInput::Engine { kind, payload }` unless
//! core must understand their semantics to enforce a kernel mechanism.
//!
//! Protocols follow the same boundary: core owns the `HostTurnProtocol` state
//! shape and the `ProtocolDriverPlugin` slot, while external protocol crates
//! provide the driver implementation.

/// Re-exported so `impl_noop_attachment_manifest!` can paste an
/// `#[async_trait]` impl into crates that do not depend on `async-trait`
/// directly. Not part of the supported surface.
#[doc(hidden)]
pub use async_trait::async_trait;
/// Re-exported so every `RuntimeEffectController` implementation can spell
/// `await_next_settlement`'s cancellation parameter without taking a direct
/// `tokio-util` dependency of its own (FIG-2266).
pub use tokio_util::sync::CancellationToken;

pub use crate::runtime::concrete_turn_cancellation_authority;
pub use lash_core_store::attachments;
pub use lash_core_store::chronological;
pub use lash_core_store::impl_noop_attachment_manifest;
pub use lash_core_store::protocol_turn_options::{ProtocolTurnOptions, ProtocolTurnOptionsError};
mod backend;
pub use backend::{Backend, BackendQueuedWork, StoreSet};
pub mod direct;
pub mod direct_completion_client;
pub mod engine;
pub(crate) use lash_core_ids::identity_json;
pub use lash_core_llm::llm;
pub(crate) use lash_core_llm::model;
pub mod model_clamp;
pub(crate) use lash_core_ids::operational_metrics;
pub(crate) use model_clamp::ModelGenerationClamp;
/// Panic containment for runtime-owned work.
///
/// The module lives in `lash-core-ids`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helpers crate-internal.
pub mod panic_containment {
    pub(crate) use lash_core_ids::panic_containment::{
        enforce_loudness, enforce_message, payload_message,
    };
    pub use lash_core_ids::panic_containment::{is_loud, set_loud};
}
#[cfg(feature = "perf-witness")]
pub use lash_core_ids::perf_witness;
pub mod plugin;
pub mod plugin_stack;
pub mod process_registry;
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
/// Standard-lock poison recovery traits used across Lash hosts and runtimes.
pub mod sync {
    pub use lash_sansio::sync::*;
}
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_ids::test_watchdog;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tool_dispatch;
pub mod tool_intent;
pub mod tool_provider;
pub mod tool_registry;
pub mod tool_result;
pub mod trace;
pub mod triggers;

pub mod facade_support {
    pub use crate::runtime::effect::bind_store_turn_control_authority;
    pub use crate::runtime::effect::{
        LiveOpenerContext, LiveOpenerGuard, LiveOpenerRegistry, ToolChildDriver, ToolChildHost,
        opener_for_execution_scope,
    };
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
    /// Facade-internal configuration for a process-local turn token.
    pub fn configure_local_turn_token(hint: &crate::TurnCancelOriginHint, origin: Option<String>) {
        hint.configure_local_token(origin);
    }
    pub use crate::tool_provider::orchestration::{
        OrchestratingToolDef, OrchestratingToolImplementation, OrchestrationContext,
    };
    pub fn build_core_tool_registry(
        host: &crate::plugin::PluginHost,
    ) -> Result<std::sync::Arc<crate::ToolRegistry>, crate::PluginError> {
        host.build_core_tool_registry()
    }

    pub fn tool_registry_manifests(registry: &crate::ToolRegistry) -> Vec<crate::ToolManifest> {
        crate::ToolProvider::tool_manifests(registry)
    }

    pub fn resolve_tool_registry_contract(
        registry: &crate::ToolRegistry,
        name: &str,
    ) -> Option<std::sync::Arc<crate::ToolContract>> {
        registry.resolve_catalog_contract(name)
    }

    pub use crate::attachments::AttachmentGcFence;
    pub use crate::attachments::AttachmentReclamationPolicy;
    pub use crate::attachments::AttachmentReclamationReport;
    pub use crate::attachments::EmptyRootSetPolicy;
    pub use crate::attachments::FileAttachmentStore;
    pub use crate::attachments::InMemoryAttachmentStore;
    pub use crate::attachments::SessionAttachmentStore;
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
    pub use crate::plugin::AbortTurnDirective;
    pub use crate::plugin::AfterToolCallPluginDirective;
    pub use crate::plugin::AfterTurnPluginDirective;
    pub use crate::plugin::AssistantResponseTransform;
    pub use crate::plugin::BeforeToolCallPluginDirective;
    pub use crate::plugin::CheckpointHookContext;
    pub use crate::plugin::CompactionContext;
    pub use crate::plugin::ContextCompaction;
    pub use crate::plugin::ContextCompactor;
    pub use crate::plugin::ContextError;
    pub use crate::plugin::DirectCompletion;
    pub use crate::plugin::DirectLlmCompletion;
    pub use crate::plugin::EnqueueMessagesDirective;
    pub use crate::plugin::NoPresentationArtifacts;
    pub use crate::plugin::PersistentRuntimeServices;
    pub use crate::plugin::PluginCommand;
    pub use crate::plugin::PluginDirective;
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
    pub use crate::plugin::PluginSpec;
    pub use crate::plugin::PluginSpecFactory;
    pub use crate::plugin::PluginTask;
    pub use crate::plugin::PromptHookContext;
    pub use crate::plugin::RecordedSessionConfig;
    pub use crate::plugin::ReplaceToolArgsDirective;
    pub use crate::plugin::SessionConfigChangedContext;
    pub use crate::plugin::SessionCreationConfig;
    pub use crate::plugin::SessionHandle;
    pub use crate::plugin::SessionLifecycleService;
    pub use crate::plugin::SessionObserverIntent;
    pub use crate::plugin::SessionParam;
    pub use crate::plugin::SessionPlugin;
    pub use crate::plugin::SessionStateChangedContext;
    pub use crate::plugin::ShortCircuitToolDirective;
    pub use crate::plugin::ToolCatalogContribution;
    pub use crate::plugin::ToolPresentationArtifacts;
    pub use crate::plugin::ToolPresentationInput;
    pub use crate::plugin::ToolPresentationStep;
    pub use crate::plugin::ToolResultProjectionContext;
    pub use crate::plugin::TurnContextTransform;
    pub use crate::plugin::TurnHookContext;
    pub use crate::plugin::TurnHookReport;
    pub use crate::plugin::TurnPluginDirective;
    pub use crate::plugin::TurnResultHookContext;
    pub use crate::plugin::TurnTransformContext;
    pub use crate::plugin::{KeyRejection, PluginStateEdit, PluginStateError, PluginStateStore};
    pub use crate::plugin_stack::PluginStack;
    pub use crate::provider::CacheRetention;
    pub use crate::provider::GenerationRetryGuarantee;
    pub use crate::provider::LlmTimeouts;
    pub use crate::provider::ModelEffortValidationCategory;
    pub use crate::provider::Provider;
    pub use crate::provider::ProviderComponents;
    pub use crate::provider::ProviderHandle;
    pub use crate::provider::ProviderOptions;
    pub use crate::provider::ReconciledUsage;
    pub use crate::provider::SingleProviderResolver;
    pub use crate::runtime::AgentFrameRun;
    pub use crate::runtime::AssembledTurn;
    pub use crate::runtime::AssistantOutput;
    pub use crate::runtime::CanonicalRuntimeEffectEnvelope;
    pub use crate::runtime::DirectCompletionClient;
    pub use crate::runtime::EmbeddedRuntimeHost;
    pub use crate::runtime::EventSink;
    pub use crate::runtime::InMemoryProcessExecutionEnvStore;
    pub use crate::runtime::InMemorySessionStore;
    pub use crate::runtime::InMemorySessionStoreFactory;
    pub use crate::runtime::NativeEffectHost;
    pub use crate::runtime::NativeRuntimeEffectController;
    pub use crate::runtime::NoopTurnActivitySink;
    pub use crate::runtime::ObservedProcess;
    pub use crate::runtime::ObservedProcessEvent;
    pub use crate::runtime::ObservedProcessEventLite;
    pub use crate::runtime::ObservedProcessEventPage;
    pub use crate::runtime::ObservedProcessEventReadOutcome;
    pub use crate::runtime::ObservedWorkItem;
    pub use crate::runtime::ObservedWorkItemState;
    pub use crate::runtime::OutputState;
    pub use crate::runtime::PROCESS_LEASE_SCHEMA_VERSION;
    pub use crate::runtime::ProcessAdmissionDeferred;
    pub use crate::runtime::ProcessAdmissionIntake;
    pub use crate::runtime::ProcessAdmissionReport;
    pub use crate::runtime::ProcessChangeHub;
    pub use crate::runtime::ProcessDrainDeferred;
    pub use crate::runtime::ProcessDrainReport;
    pub use crate::runtime::ProcessEngineProcessContext;
    pub use crate::runtime::ProcessEngineRegistry;
    pub use crate::runtime::ProcessEventAppendPlan;
    pub use crate::runtime::ProcessEventSink;
    pub use crate::runtime::ProcessEventSinkRegistration;
    pub use crate::runtime::ProcessRecoveryAttemptOutcome;
    pub use crate::runtime::ProcessRecoveryOperation;
    pub use crate::runtime::ProcessRuntimeHost;
    pub use crate::runtime::ProcessStartPlan;
    pub use crate::runtime::ProcessTerminalSemantics;
    pub use crate::runtime::ProcessToolVisibilityFilter;
    pub use crate::runtime::ProcessTransition;
    pub use crate::runtime::ProcessTransitionPlan;
    pub use crate::runtime::ProcessTurnCancellation;
    pub use crate::runtime::ProcessWake;
    pub use crate::runtime::ProcessWakeDeliveryRequest;
    pub use crate::runtime::ProcessWorkObserver;
    pub use crate::runtime::ProcessWorkSnapshot;
    pub use crate::runtime::ProcessWorkerFault;
    pub use crate::runtime::QueuedDrainCandidate;
    pub use crate::runtime::QueuedDrainPolicy;
    pub use crate::runtime::QueuedDrainRequest;
    pub use crate::runtime::QueuedDrainSelection;
    pub use crate::runtime::QueuedWorkAuthority;
    pub use crate::runtime::QueuedWorkBatchingConfig;
    pub use crate::runtime::QueuedWorkClaimPolicy;
    pub use crate::runtime::QueuedWorkKind;
    pub use crate::runtime::ReconciledUsageAttempt;
    pub use crate::runtime::RuntimeAwaitEventOptions;
    pub use crate::runtime::RuntimeEffectReplayTrace;
    pub use crate::runtime::RuntimeHostConfig;
    pub use crate::runtime::RuntimeSleepOptions;
    pub use crate::runtime::SessionCommand;
    pub use crate::runtime::SessionCommandReceipt;
    pub use crate::runtime::SessionScopeId;
    pub use crate::runtime::SessionUsageReport;
    pub use crate::runtime::SystemClock;
    pub use crate::runtime::TerminationPolicy;
    pub use crate::runtime::TurnActivitySink;
    pub use crate::runtime::TurnAddress;
    pub use crate::runtime::TurnAttach;
    pub use crate::runtime::TurnCancelAffectedInput;
    pub use crate::runtime::TurnCancelClosureAuthorization;
    pub use crate::runtime::TurnCancelClosureAuthorizationOutcome;
    pub use crate::runtime::TurnCancelClosureProposal;
    pub use crate::runtime::TurnCancelClosureSettlement;
    pub use crate::runtime::TurnCancelDisposition;
    pub use crate::runtime::TurnCancelInputOutcome;
    pub use crate::runtime::TurnCancelIntentSnapshot;
    pub use crate::runtime::TurnCancelMode;
    pub use crate::runtime::TurnCancelOutcome;
    pub use crate::runtime::TurnCancelReceipt;
    pub use crate::runtime::TurnCancelRequest;
    pub use crate::runtime::TurnCancelRequestRecord;
    pub use crate::runtime::TurnCancellationAuthority;
    pub use crate::runtime::TurnCancellationEvidence;
    pub use crate::runtime::TurnControlAttachment;
    pub use crate::runtime::TurnControlAuthorityOwner;
    pub use crate::runtime::TurnExecutionMetrics;
    pub use crate::runtime::TurnInputAcceptanceReceipt;
    pub use crate::runtime::TurnIssue;
    pub use crate::runtime::TurnIssueSeverity;
    pub use crate::runtime::TurnTerminal;
    pub use crate::runtime::TurnWorkDriver;
    pub use crate::runtime::UnreportedUsageAttempt;
    pub use crate::runtime::UsageReconciliationReport;
    pub use crate::runtime::UsageReportRow;
    pub use crate::runtime::UsageTotals;
    pub use crate::runtime::WakeDeliveryDriveReport;
    pub use crate::runtime::WakeDeliveryDriver;
    pub use crate::runtime::WatchedRegistry;
    pub use crate::runtime::await_event_coordinator;
    pub use crate::runtime::current_epoch_ms;
    pub use crate::runtime::diff_token_ledger;
    pub use crate::runtime::diff_usage_reports;
    pub use crate::runtime::effect::executor::control::facade_ops::ScopedEffectControllerFacadeOps;
    pub use crate::runtime::effect_replay_driver;
    pub use crate::runtime::process_runtime_session_ids;
    pub use crate::runtime::process_signal_event_type;
    pub use crate::runtime::process_signal_wait_key;
    pub use crate::runtime::process_wake_delivery;
    pub use crate::runtime::process_wake_source_key;
    pub use crate::runtime::promise_semantics;
    pub use crate::runtime::reconcile_pruned_trigger_deliveries;
    pub use crate::runtime::refuse_unhonored_group_membership;
    pub use crate::runtime::registry_transitions;
    pub use crate::runtime::release_process_execution_permit_while;
    pub use crate::runtime::turn_control_binding_id_for_scope;
    pub use lash_core_store::protocol_turn_options::facade_ops::ProtocolTurnOptionsFacadeOps;
    pub use lash_core_store::session_identity::facade_ops::AgentFrameReasonFacadeOps;
    pub use lash_core_store::turn_input_vocabulary::facade_ops::TurnContextFacadeOps;
    pub const RUNTIME_TUNING_METRICS_ENABLED: bool = cfg!(feature = "otel-trace");
    /// Record one first-party PostgreSQL runtime-connection acquisition wait.
    pub fn record_postgres_pool_acquire_wait(wait: std::time::Duration, outcome: &'static str) {
        crate::operational_metrics::record_postgres_pool_acquire_wait(wait, outcome);
    }
    pub use crate::runtime::terminal_append_request;
    pub use crate::runtime::validate_generic_process_event_append;
    pub use crate::runtime::validate_replayed_effect_envelope;
    pub use crate::runtime::watch_process_registry;
    pub use crate::runtime::watch_process_registry_with_sink;
    pub use crate::runtime::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};
    pub use crate::session::InjectedTurnInput;
    pub use crate::session::ToolInvocation;
    pub use crate::session::ToolInvocationReply;
    pub use crate::session_graph::frame_node_id;
    pub use crate::session_model::ConversationRecord;
    pub use crate::session_model::GenerationOverlay;
    pub use crate::session_model::SessionSpec;
    pub use crate::session_model::context::PreparedContext;
    pub use crate::store::LeaseTimings;
    pub use crate::store::LeaseTimingsError;
    pub use crate::store::SessionHead;
    pub use crate::store::{CommitBudget, CommitBudgetLimit};
    pub use crate::tool_intent::legacy_tool_intent_v1_lookup_key;
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use crate::tool_provider::ToolTriggerClient;
    pub use crate::tool_registry::PLUGIN_TOOL_SOURCE_ID;
    pub use crate::tool_registry::ReconfigureError;
    pub use crate::tool_registry::SupersededToolIdentity;
    pub use crate::tool_registry::ToolRestoreReport;
    pub use crate::tool_registry::ToolSourceHandle;
    pub use crate::tool_registry::ToolSourcePolicy;
    pub use crate::tool_registry::ToolStateEntry;
    pub use crate::tool_registry::ToolSurfaceOpenMode;
    pub use crate::tool_registry::facade_ops::ToolRegistryFacadeOps;
    pub use crate::triggers::InMemoryTriggerStore;
    pub use crate::triggers::TriggerDeliveryEmitOutcome;
    pub use crate::triggers::TriggerDeliveryEmitReceipt;
    pub use crate::triggers::TriggerEmitReport;
    pub use crate::triggers::TriggerEvent;
    pub use crate::triggers::TriggerEventType;
    pub use crate::triggers::TriggerRegistration;
    pub use crate::triggers::TriggerRouter;
    pub use crate::triggers::TriggerTarget;
    pub use crate::triggers::default_trigger_source_key;
    pub use crate::triggers::derived_trigger_subscription_key;
    pub use crate::triggers::deterministic_delivery_process_id;
    pub use crate::triggers::deterministic_occurrence_id;
    pub use crate::triggers::deterministic_subscription_id;
    pub use crate::triggers::empty_trigger_source_key;
    pub use crate::triggers::evaluate_trigger_mutation;
    pub use crate::triggers::evaluate_trigger_mutation_with_incarnation;
    pub use crate::triggers::evaluate_trigger_prune;
    pub use crate::triggers::next_trigger_revision;
    pub use crate::triggers::next_trigger_store_revision;
    pub use crate::triggers::sort_trigger_delivery_reservations;
    pub use crate::triggers::trigger_command_fingerprint;
    pub use crate::triggers::trigger_occurrence_request_matches_record;
    pub use crate::triggers::trigger_operation_receipt_id;
    pub use crate::triggers::validate_trigger_occurrence_request;
    pub use lash_core_store::session_graph::facade_ops::{
        SessionGraphFacadeOps, SessionNodeProjection,
    };
    pub use lash_core_store::session_state::facade_ops::RuntimeSessionStateFacadeOps;
    pub use lash_core_store::tool_state::facade_ops::ToolStateFacadeOps;
    pub use lash_sansio::AcceptedInjectedTurnInput;
    pub use lash_sansio::AttachmentMaterializationNotice;
    pub use lash_sansio::AttachmentMaterializationReason;
    pub use lash_sansio::AttachmentMaterializationSource;
    pub use lash_sansio::AttachmentRef;
    pub use lash_sansio::EffectId;
    pub use lash_sansio::ErrorEnvelope;
    pub use lash_sansio::MessageSequence;
    pub use lash_sansio::ModelToolReturn;
    pub use lash_sansio::ModelToolReturnPart;
    pub use lash_sansio::ProviderSchemaCapabilities;
    pub use lash_sansio::ResolvedSchema;
    pub use lash_sansio::Response;
    pub use lash_sansio::SchemaPurpose;
    pub use lash_sansio::SchemaResolutionError;
    pub use lash_sansio::SchemaResolutionRequest;
    pub use lash_sansio::SessionStreamEvent;
    pub use lash_sansio::ToolCatalogBuildError;
    pub use lash_sansio::TurnFinish;
    pub use lash_sansio::TurnOutcome;
    pub use lash_sansio::TurnStop;
    pub use lash_sansio::append_assistant_text_part;
    pub use lash_sansio::build_tool_catalog;
    pub use lash_sansio::default_prompt_template;
    pub use lash_sansio::head_tail_truncate;
    pub use lash_sansio::normalized_response_parts;
    pub use lash_sansio::reasoning_part;
    pub use lash_sansio::render_turn_causes_prompt;
    pub use lash_sansio::resolve_schema;
    pub use lash_sansio::shared_parts;
    pub use lash_sansio::visible_response_text_from_parts;
    pub use lash_trace::JsonlTraceSink;
    pub use lash_trace::TraceBranchSelection;
    pub use lash_trace::TraceLabelMetadata;
    pub use lash_trace::TraceLevel;
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
pub(crate) use crate::attachments::{
    AttachmentProducer, AttachmentSourcePolicy, OpenAttachmentSourcePolicy,
};
pub(crate) use crate::plugin::{
    RuntimeServices, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
    SessionObserverIntent,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use crate::runtime::UnavailableProcessService;
pub(crate) use lash_sansio::{ToolCatalogBuildInput, validate_tool_input};

pub mod sansio {
    pub use lash_sansio::sansio::{
        ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
        ContextProjector, EffectId, ExecutionEnvironmentSync, LlmCallError, PendingToolCall,
        ProjectorTurnInputs, ProtocolDriverHandle, Response, TurnCause, TurnMachine,
        WaitingExecState, WaitingLlmState, render_turn_causes_prompt,
    };
}

pub use attachments::{
    AttachmentGcFence, AttachmentReclamationPolicy, AttachmentRootSet, AttachmentStore,
    AttachmentStoreError, AttachmentStoreFailureClass, AttachmentStorePersistence,
    EmptyRootSetPolicy, StoredAttachment, StoredBlobRef,
};
pub use lash_sansio::llm::types::{
    AttachmentSource, AttemptOutcome, AttemptRecord, AttemptUsageDisposition, ChargeSafetyDecision,
    ChargeSafetyDenialReason, ExecutionEvidence, ExecutionEvidenceCollectionInterruption,
    ExecutionEvidenceMergeError, GenerationOptionOutcome, GenerationOptions, GenerationReceipt,
    LlmCallId, LlmCallRecord, LlmOutputPart, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvidence, LlmTerminalReason, NonNegativeFiniteF64, NormalizedError, ProtocolPosition,
    ProviderEndpointError, ProviderFileScope, ProviderReplayDrop, ProviderReplayDropReason,
    ProviderReplayKind, ProviderRouteIdentity, RetryDecision,
};
pub use lash_sansio::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, BatchId,
    CancelOrigin, CancelRequest, CellFailure, CellFailureKind, CheckpointDelivery, CheckpointKind,
    CompactToolContract, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason, ExecResponse,
    ExecutedCall, ExecutedCallOutcome, ExecutedCallRecord, FrameKey, FrameKeyError, InputId,
    LashSchema, LlmCallError, MediaType, Message, MessageOrigin, MessageRole, NodeId, Observation,
    ObservedProcessFailure, OmittedToolCalls, Part, PartKind, PluginMessage, PluginRuntimeEvent,
    ProjectionMode, PromptBuiltin, PromptContribution, PromptContributionBody,
    PromptContributionGate, PromptLayer, PromptSlot, PromptSlotLayer, PromptTemplate,
    PromptTemplateEntry, PromptTemplateSection, SchemaContract, SchemaProjectionOverride,
    SchemaProjectionPolicy, SessionAppendNode, TextProjectionMetadata, TokenUsage,
    TokenUsageOverflow, ToolActivation, ToolArgumentProjectionPolicy, ToolCallOutcome,
    ToolCallOutput, ToolCallRecord, ToolCancellation, ToolCatalog, ToolCatalogBuildError,
    ToolCatalogEntry, ToolContract, ToolControl, ToolDefinition, ToolDiscovery, ToolFailure,
    ToolFailureClass, ToolFailureSource, ToolId, ToolIntentExecutionOutcome, ToolIntentIdentity,
    ToolIntentKind, ToolIntentRefusalReason, ToolManifest, ToolOutputContract, ToolRetryPolicy,
    ToolRetryStatus, ToolValue, TurnCause, TurnId, TurnOutputSource,
};
pub use tool_provider::{
    ToolAttachmentClient, ToolDirectCompletionClient, ToolDispatchClient, ToolProcessEventClient,
    ToolSessionAdmin, ToolSessionModel,
};
/// Project a successful tool control into its terminal turn outcome.
///
/// Agent-frame seeds are typed at their serde boundary, so a terminal outcome
/// can never advertise nodes that the commit materializer would have to drop.
///
/// # Integrator class
///
/// Protocol-engine implementors use this shared projection to preserve the
/// host's terminal-outcome semantics.
pub fn turn_outcome_from_tool_control(
    tool_name: &str,
    control: &ToolControl,
) -> Option<TurnOutcome> {
    match control {
        ToolControl::SwitchAgentFrame {
            frame_key,
            initial_nodes,
            task: Some(task),
        } if !task.trim().is_empty() => Some(TurnOutcome::AgentFrameSwitch {
            frame_key: frame_key.clone(),
            task: task.clone(),
            initial_nodes: initial_nodes.clone(),
        }),
        ToolControl::Finish { value } => Some(TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: tool_name.to_string(),
            value: tool_value_for_projection(value),
        })),
        ToolControl::Fail { failure } => Some(TurnOutcome::Stopped(TurnStop::ToolError {
            tool_name: tool_name.to_string(),
            value: tool_failure_for_projection(failure),
        })),
        ToolControl::SwitchAgentFrame { .. } => None,
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
pub use protocol_build::ProtocolBuildInput;
pub use tool_registry::{
    SupersededToolIdentity, ToolRegistry, ToolRestoreReport, ToolSourcePolicy, ToolState,
    ToolSurfaceOpenMode,
};
pub use tool_result::{
    CancelHint, PendingAnnouncement, PendingCompletion, PendingResolver, TimeoutBehavior,
    ToolOutcome,
};
pub use triggers::{
    TriggerCommand, TriggerCommandOutcome, TriggerDeliveryReservation,
    TriggerDeliveryReservationOutcome, TriggerDeliveryRetentionCandidate, TriggerEffectResult,
    TriggerEventCatalog, TriggerIngressReceipt, TriggerInputBinding, TriggerMutationOutcome,
    TriggerMutationReceipt, TriggerOccurrenceFilter, TriggerOccurrenceOutcome,
    TriggerOccurrenceReclamationReport, TriggerOccurrenceReclamationResult,
    TriggerOccurrenceRecord, TriggerOccurrenceRequest, TriggerOperationError, TriggerOwnerScope,
    TriggerProviderRoute, TriggerRetentionReconciliationReport, TriggerRouteRefusal,
    TriggerRouteRestorer, TriggerSourceCapture, TriggerStore, TriggerSubscriptionDraft,
    TriggerSubscriptionFilter, TriggerSubscriptionRecord, admit_trigger_registration_target,
};

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
    type Event = crate::session_model::ProtocolEvent;
    type Termination = ProtocolTurnOptions;
    type DriverState = ProtocolDriverState;
}

/// Host-specialized effect vocabulary for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors drive these effects; applications use the facade.
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
/// Protocol-engine implementors complete preparation before driving the machine.
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
/// Protocol-engine implementors drive this machine; applications use the facade.
pub type TurnMachine = lash_sansio::TurnMachine<HostTurnProtocol>;
/// Host turn-machine configuration for protocol-engine implementors.
///
/// # Integrator class
///
/// Protocol-engine implementors construct this configuration at their boundary.
pub type TurnMachineConfig = lash_sansio::TurnMachineConfig<HostTurnProtocol>;
pub use lash_sansio::{FailureCode, TurnFailureCode, TurnFailureKind};
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{OtelTraceOptions, OtelTraceSink};
pub use lash_trace::{
    TraceAttachment, TraceChargeSafetyDecision, TraceChargeSafetyDenialReason, TraceContentBlock,
    TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceLlmResponse, TracePromptComponent, TraceProviderReplayDropEvent,
    TraceProviderReplayDropReason, TraceProviderReplayKind, TraceProviderRequestEvent,
    TraceProviderRouteIdentity, TraceProviderStreamEvent, TraceRuntimeStreamEvent, TraceTokenUsage,
    TraceToolResultBlock, TraceToolSpec,
};
pub use llm::transport::ProviderFailureKind;
pub use model::{ModelLimits, ModelLimitsError, ModelSpec, ModelSpecBuilder};
pub use plugin::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, AppendSessionNodesOutcome,
    AppendSessionNodesRequest, FrameNodeId, FrameNodeIdError, KeyRejection, PluginError,
    PluginExtensions, PluginNamespaceState, PluginOptions, PluginState, PluginStateEdit,
    PluginStateError, PluginStateStore, ProcessEngineContributionContext,
    ProtocolBeforeLlmCallContext, ProtocolLlmCallAction, SESSION_PLUGIN_INIT_MAX_BYTES,
    SessionCreateRequest, SessionGraphService, SessionLineage, SessionPluginInit,
    SessionPluginSource, SessionReadView, SessionRelation, SessionSnapshot, SessionStartPoint,
    SessionStateService, SessionToolAccess, SessionToolAccessError, SubagentSessionContext,
    SwitchAgentFrameRequest, durable_identity_conflict, is_durable_identity_conflict,
};
pub use plugin::{OpenAgentFrameRequest, OpenAgentFrameResult};
pub use provider::{
    AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
    AttachmentCapabilitySnapshot, AttachmentMimeSource, CacheControlDialect, GoogleDialect,
    InstructionRole, ModelCapability, OpenAiReasoningContext, ReasoningCapability,
    ReasoningDisableEncoding, ReasoningEncoding, ReasoningRetentionCapability,
    ReasoningRetentionPolicy, ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
    ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability, StreamTermination,
};
pub(crate) use provider::{
    EmptyProviderResolver, ProviderResolutionError, RuntimeProviderResolver,
};
#[cfg(any(test, feature = "testing"))]
pub use runtime::ConformanceProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use runtime::ProcessEventLogTestSupport;
#[cfg(any(test, feature = "testing"))]
pub use runtime::ProcessRegistryTestSupport;
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestLocalProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestProcessRegistryWriteExt;
pub(crate) use runtime::default_queued_drain_policy;
#[cfg(any(test, feature = "testing"))]
pub use runtime::fail_parent_end_once;

// This block includes the effect / process-control types consumed by external
// effect hosts (e.g. lash-restate's workflows) and their integration tests —
// they are deliberately public; the rest of the runtime module stays
// crate-internal.
pub use process_registry::{
    InMemoryProcessDefinitionRegistry, ProcessDefinitionExpectation, ProcessDefinitionLifecycle,
    ProcessDefinitionRecord, ProcessDefinitionRegistration, ProcessDefinitionRegistry,
};
pub(crate) use runtime::ToolAttemptEffectOutcome;
pub use runtime::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AcceptedTurnInputDrive,
    AcceptedTurnInputRefusal, AdmittedProcessIdentity, AdmittedScope, AdmittedScopeError,
    ArtifactOwner, AssistantResponseHookEvents, AssistantStreamHookState, AwaitEventKey,
    AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, CausalRef,
    ChargeSafetyRefusalEvidence, CheckpointClaimSet, ChildDrainOutcome, Clock, ClockWallTime,
    CommandJournalGuard, CommandReplayKey, CompletionKeyPreparation, DeclaredProcessIdentity,
    DeliveryPolicy, DrainMode, DrainModePolicy, DrainedChild, EffectAddress,
    EffectGroupDrainBudget, EffectGroupHandle, EffectGroupMembership, EffectHost,
    EffectJournalRetirement, EffectJournaling, EffectOpener, EffectOpenerError,
    EffectRetirementGate, ExecutionScope, ForkPoint, ForkSessionReceipt, ForkSessionRequest,
    GroupChildBinding, GroupDrainReport, GroupExecutors, GroupFinalizationReport,
    GroupOnlyFinalization, GroupReopen, GroupSettlement, GroupWakePolicy, HandleId,
    InMemoryProcessExecutionEnvStore, InputItem, LedgerUsageDisposition, LlmRequestSpec,
    LlmStreamRecord, LoserPolicy, NativeProcessWork, NativeSubstrateConfig,
    NativeSubstrateConfigError, NoQueuedWork, OnParentEnd, OpenerFinalizationSteps,
    PARENT_SCOPE_STORAGE_PAYLOAD_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
    PROCESS_WAKE_MERGE_KEY, ParentEndPlan, ParentScope, ParentScopeStorageError, PendingTurnInput,
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputClaimDiagnostics, PendingTurnInputDraft, PendingTurnInputRead,
    PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome, PersistedSegmentHandover,
    ProcessArtifactCleanup, ProcessArtifactCleanupAck, ProcessAwaitOutput, ProcessCancelReceipt,
    ProcessChange, ProcessChangeCursor, ProcessClockRebind, ProcessCommand,
    ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessContinuationStore,
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
    ProcessDefinitionValue, ProcessEffectOutcome, ProcessEngine, ProcessEngineAdmission,
    ProcessEngineKind, ProcessEngineRegistration, ProcessEngineRegistry, ProcessEngineRunContext,
    ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleView, ProcessId, ProcessIdentity, ProcessIncarnation,
    ProcessInfraError, ProcessInput, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessLeaseSchemaVersionError, ProcessLeases, ProcessLifecycle,
    ProcessLifecyclePolicy, ProcessListFilter, ProcessListMode, ProcessLiveReferenceView,
    ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope, ProcessOriginator,
    ProcessOriginatorFilter, ProcessOutcome, ProcessOutcomeObserver, ProcessProvenance,
    ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRef, ProcessRegistrar,
    ProcessRegistration, ProcessRegistrationDisposition, ProcessRegistrationOutcome,
    ProcessRegistrationProbe, ProcessRegistry, ProcessRegistryBinding, ProcessResumeRefusal,
    ProcessRetention, ProcessRunOutcome, ProcessScopeFenceHosts, ProcessSegmentKey, ProcessService,
    ProcessSessionDeleteReport, ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTerminalSpec, ProcessTerminalWait, ProcessTombstone,
    ProcessToolIntents, ProcessValueSelector, ProcessWakeDelivery, ProcessWakeOutbox,
    ProcessWakeSpec, ProcessWorkSubstrate, ProcessWorkWiring, ProcessWorklistCursor,
    ProcessWorklistPage, ProjectionWatermark, ProtocolSessionExtension,
    ProtocolSessionExtensionHandle, ProtocolTurnExtension, ProtocolTurnExtensionHandle,
    QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection,
    QueuedLaneAcquisition, QueuedLaneAttempt, QueuedLaneGuard, QueuedLaneHolder, QueuedLaneProbe,
    QueuedWorkAuthority, QueuedWorkBatchingConfig, QueuedWorkClaimPolicy, QueuedWorkKind,
    QueuedWorkSubstrate, RecordedJournal, RecordedKeyFence, RecordedKeyRange, RecordedKeys,
    RecoveryContract, Resolution, ResolveOutcome, RuntimeAttribution, RuntimeCheckpointComponents,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport,
    RuntimeError, RuntimeErrorCause, RuntimeErrorCode, RuntimeInvocation, RuntimeReplay,
    RuntimeReplayAttribution, RuntimeSessionState, ScopeBoundController, ScopedEffectController,
    SegmentHandover, SegmentProgress, SegmentStartMarker, ServedOnlyFence, SessionDrainOutcome,
    SessionId, SessionListFilter, SessionRelationKind, SessionScope, SessionStateVersionRefusal,
    SessionStoreCreateRequest, SessionStoreFactory, SessionSummary, SessionWorkTarget, SleepSpec,
    StoreEffectGroupClosing, StoreEffectGroupDrain, StoreRealization, TokenLedgerEntry,
    ToolAttemptLaunch, ToolIntentOutcomeSink, ToolIntentPreparation, ToolIntentSubmissionGuard,
    TurnActivity, TurnActivityId, TurnCancelAffectedInput, TurnCancelClosureAuthorization,
    TurnCancelClosureAuthorizationOutcome, TurnCancelClosureOwnerBinding,
    TurnCancelClosureProposal, TurnCancelClosureSettlement, TurnCancelDisposition,
    TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode, TurnCancelOriginHint,
    TurnCancelRequestRecord, TurnCancellationAuthority, TurnContext, TurnControlAttachment,
    TurnControlAuthorityOwner, TurnControlBinding, TurnControlBindingId, TurnControlBindingIdError,
    TurnEvent, TurnFailureCause, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement, TurnInput, TurnInputApplication, TurnInputCheckpointBoundary,
    TurnInputClaim, TurnInputClaimData, TurnInputClaimMode, TurnInputCompletion,
    TurnInputCompletionData, TurnInputIngress, TurnInputSettlementClaim, TurnInputState,
    UnreportedLedgerAttempt, UnsettledEffectGroup, UsageDispositionError, WaitKind, WaitState,
    WakeDelivery, WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome, WakeDeliveryConfig,
    WakeDeliveryDisposition, WakeDeliveryReport, WakeDeliveryState, WakeDiscardReason,
    WatchedRegistry, WorkCadencePolicy, WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier,
    WorkerSweepPolicy, admit_session_state_generation, effect_groups_unsupported,
    ensure_process_lease_schema_version,
};
#[allow(unused_imports)]
pub(crate) use runtime::{
    ProcessEventSemantics, QueuedCheckpointTurnInput, QueuedCheckpointWork, QueuedTurnWork,
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkClaimData, QueuedWorkCompletion, QueuedWorkCompletionData, QueuedWorkEnqueueOutcome,
    QueuedWorkItem, QueuedWorkPayload, RuntimeSubject, TurnWorkPayload,
    artifact_owner_is_permanently_retired, artifact_staging_owner_edge_is_missing,
    load_process_execution_env, materialize_process_event_semantics, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_event_invocation, process_registration_fingerprint, process_wake_batch_draft,
    process_wake_input_from_event_payload, process_wake_turn_cause, process_wake_turn_text,
    publish_process_execution_env, require_event_replay, settle_started_process_engine_artifacts,
    settle_started_process_execution_env,
};
pub(crate) use session::Session;
pub use session::{ExecRequest, RuntimeExecutionContext, SessionError};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SESSION_NODE_BODY_SCHEMA_VERSION, SessionGraph,
    SessionGraphScopeError, SessionNodePayload, SessionNodeRecord,
};
pub(crate) use session_model::RuntimeSessionPolicy;

pub use session_model::{ChargeSafetyPolicy, NoProgressBudget, SessionPolicy, TurnBudget};
pub use session_model::{ProtocolEvent, SessionHistoryRecord};
pub use store::{
    AppendRequestIdentity, AttachmentCondemnation, AttachmentCondemnationPhase,
    AttachmentCondemnationProvenance, AttachmentCondemnationRecord, AttachmentDeleteArming,
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwner,
    AttachmentOwnerKind, AttachmentWriteFence, AttachmentWritePermit, AttachmentWriteToken,
    BlobRef, CURRENT_SESSION_STATE_VERSION, CheckpointComponentDescriptor, CommitBudget,
    CommitBudgetLimit, DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface,
    GcReport, HydratedCheckpointComponent, HydratedSessionCheckpoint, LeaseClaimNonce,
    LeaseOwnerIdentity, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport,
    MaintenanceResult, MaintenanceStop, MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION,
    OperationId, OrphanedTurnInputScope, QueuedWorkClaimOutcome, QueuedWorkClaimRefusal,
    QueuedWorkStore, RetentionBound, RetentionReport, RuntimeCommit, RuntimePersistence,
    RuntimeTurnCommitStamp, RuntimeUsageDelta, RuntimeUsageDeltaIdentity, ScanCoverage,
    SelectedQueuedWorkClaimOutcome, SemanticBoundaryOperation, SessionAdmission, SessionBinding,
    SessionBlobReclaimReport, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseDisplacement,
    SessionExecutionLeaseObservation, SessionExecutionLeaseRenewalInstallMismatch,
    SessionExecutionLeaseStore, SessionMeta, SessionStateAdmission, StoreBackend,
    StoreComponentVersion, StoreError, StoreMaintenance, StorePreflight, StoreReleaseStamp,
    StoreReleaseState, StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus,
    StoreSchemaVerdict, TurnCancelRepairDecision, TurnCancelRepairResult, TurnInputStore,
    VacuumReport, WorkClaim, WorkCompletion, compare_releases, release_stamp_advances,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphAppend, PersistedSessionRead, RuntimeCommitReceipt, SessionCheckpoint, SessionHeadMeta,
    SessionHeadPayload, ensure_supported_schema_version, load_persisted_session_state,
};
pub use tool_intent::{
    CancelProcessIntent, EmitProcessEventIntent, EmitTriggerIntent,
    RegisterProcessDefinitionIntent, RegisterTriggerIntent, SignalProcessIntent,
    StartProcessIntent, TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT,
    TOOL_INTENT_MAX_PER_KIND, TOOL_INTENT_PROTOCOL_V3, ToolAttemptOutcome, ToolIntent,
    ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord, ToolIntents, ToolOutcomeDone,
    derive_tool_intent_identity, derive_tool_intent_identity_under, rederive_tool_intent_identity,
};
/// Tool-provider contracts, including child-process execution observation hooks.
pub use tool_provider::{
    AttemptContext, AttemptProcessReads, AttemptSessionReads, ExternalLaunchAudit,
    InternalProcessAdmin, InternalProcessContext, InternalProcessToolCall, InternalProcessToolDef,
    InternalProcessToolImplementation, PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall,
    ToolCall, ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolContext,
    ToolExecutionGrant, ToolPrepareCall, ToolPrepareContext, ToolProvider,
};

#[doc(hidden)]
pub mod core_internal {
    pub use crate::direct_completion_client::{DirectCompletionService, DirectExecutionPosition};
    pub use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
    pub use crate::runtime::effect::executor::{sleep_duration, sleep_with_cancellation};
    pub fn attach_process_invocation_correlation(
        turn_context: &mut crate::TurnContext,
        process_id: &crate::ProcessId,
        authority: &crate::ProcessExecutionWriteAuthority,
    ) {
        crate::session::attach_process_invocation_correlation(turn_context, process_id, authority);
    }

    pub fn clear_process_invocation_correlation(turn_context: &mut crate::TurnContext) {
        crate::session::clear_process_invocation_correlation(turn_context);
    }
}
