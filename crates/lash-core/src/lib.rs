//! Runtime kernel for Lash.
//!
//! The process kernel intentionally understands `ToolCall`, `SessionTurn`, and
//! `External` because those inputs carry runtime mechanisms core must enforce:
//! recorded tool attempts, child-session turns, and externally completed work. New
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

pub use lash_core_execution::admitted_scope_wire;
pub use lash_core_execution::compat;
pub use lash_core_execution::direct;
pub(crate) use lash_core_execution::direct_completion_client;
pub use lash_core_execution::engine;
pub(crate) use lash_core_execution::model_clamp;
#[cfg(any(test, feature = "testing"))]
pub use lash_core_execution::process_id_for_test;
pub use lash_core_execution::process_id_from_handle_json;
/// Durable tool-effect format versions, re-exported for the format manifest.
pub use lash_core_execution::runtime::{
    TOOL_ATTEMPT_CAPTURE_VERSION, TOOL_CHILD_REQUEST_VERSION, TOOL_PRESENTATION_VERSION,
    TOOL_SETTLEMENT_VERSION,
};
pub use lash_core_ids::operational_metrics;
pub use lash_core_llm::llm;
pub(crate) use lash_core_llm::model;
pub use lash_core_store::attachments;
pub use lash_core_store::chronological;
pub use lash_core_store::impl_current_fleet_format;
pub use lash_core_store::impl_noop_attachment_manifest;
pub use lash_core_store::protocol_turn_options::{
    PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION, ProtocolTurnOptions, ProtocolTurnOptionsError,
};
pub use lash_core_store::surface_format;
pub(crate) use model_clamp::ModelGenerationClamp;
/// The session drive (FIG-3600): admission as recorded steps, then the
/// admitted root's turns.
pub use runtime::drive;
/// A session's durable close, the point of no return of its deletion
/// (FIG-3600 S7).
pub use runtime::session_close;
pub use runtime::session_delete;
/// Re-exported so every `RuntimeEffectController` implementation can spell
/// `await_next_settlement`'s cancellation parameter without taking a direct
/// `tokio-util` dependency of its own (FIG-2266).
pub use tokio_util::sync::CancellationToken;
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
pub use lash_core_execution::plugin;
pub(crate) use lash_core_execution::plugin_stack;
pub use lash_core_execution::process_registry;
pub(crate) use lash_core_execution::protocol_build;
#[cfg(feature = "perf-witness")]
pub use lash_core_ids::perf_witness;
/// Provider components for pluggable LLM backends.
///
/// The module lives in `lash-core-llm`; this facade re-exports its public
/// surface unchanged and keeps the crate-internal helper crate-internal.
pub mod provider {
    pub(crate) use lash_core_llm::core_internal::{
        call_id_for_scope, synthetic_terminal_call_record,
    };
    pub use lash_core_llm::provider::*;
}
pub mod runtime;
pub use lash_core_execution::session;
pub use lash_core_execution::session_model;
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
#[cfg(feature = "testing")]
pub use lash_core_execution::trace;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::trace;
pub use lash_core_execution::triggers;

pub mod facade_support {
    pub use crate::runtime::effect::{
        ContextSourceInstall, DeploymentToolChildContext, EffectGroupChildCommitOutcome,
        GroupChildFinalCommit, LiveOpenerContext, LiveOpenerGuard, LiveOpenerRegistry,
        ToolChildContextSource, ToolChildDriver, ToolChildHost, ToolChildRebuildRefusal,
        ToolChildRequest, ToolChildSessionFacts, UnrecordedSessionSources,
        opener_for_execution_scope, scope_status,
    };
    pub use crate::runtime::{DurableSessionOps, EMPTY_HEAD_REVISION};
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
    pub use crate::runtime::turn_loop::{EmptyQueuedDrainReason, QueuedTurnDrain};
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
    pub use crate::plugin::ContextPressureContext;
    pub use crate::plugin::ContextPressureDecision;
    pub use crate::plugin::ContextPressureHook;
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
    pub use crate::plugin::PluginTraceEmitter;
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
    pub use crate::plugin::ToolPresentationPresenter;
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
    pub use crate::provider::{ConfigRefusalCode, ProviderRegistry};
    pub use crate::runtime::AgentFrameRun;
    pub use crate::runtime::AssembledTurn;
    pub use crate::runtime::AssistantOutput;
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
    pub use crate::runtime::OutputState;
    pub use crate::runtime::ParkedSession;
    pub use crate::runtime::ProcessEngineProcessContext;
    pub use crate::runtime::ProcessEngineRegistry;
    pub use crate::runtime::ProcessEventAppendPlan;
    pub use crate::runtime::ProcessEventSink;
    pub use crate::runtime::ProcessEventSinkRegistration;
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
    pub use crate::runtime::QueuedDrainCandidate;
    pub use crate::runtime::QueuedDrainPolicy;
    pub use crate::runtime::QueuedDrainRequest;
    pub use crate::runtime::QueuedDrainSelection;
    pub use crate::runtime::QueuedWorkAuthority;
    pub use crate::runtime::QueuedWorkBatchingConfig;
    pub use crate::runtime::QueuedWorkKind;
    pub use crate::runtime::ReconciledUsageAttempt;
    pub use crate::runtime::RuntimeAwaitEventOptions;
    pub use crate::runtime::RuntimeEffectReplayTrace;
    pub use crate::runtime::RuntimeEnvironment;
    pub use crate::runtime::RuntimeEnvironmentBuilder;
    pub use crate::runtime::RuntimeHandle;
    pub use crate::runtime::RuntimeHostConfig;
    pub use crate::runtime::RuntimeObservation;
    pub use crate::runtime::RuntimeSleepOptions;
    pub use crate::runtime::SessionCommand;
    pub use crate::runtime::SessionCommandReceipt;
    pub use crate::runtime::SessionConfigPatch;
    pub use crate::runtime::SessionObservation;
    pub use crate::runtime::SessionObservationSubscription;
    pub use crate::runtime::SessionResume;
    pub use crate::runtime::SessionScopeId;
    pub use crate::runtime::SessionUsageReport;
    pub use crate::runtime::SystemClock;
    pub use crate::runtime::TerminationPolicy;
    pub use crate::runtime::TurnActivitySink;
    pub use crate::runtime::TurnAddress;
    pub use crate::runtime::TurnAttach;
    pub use crate::runtime::TurnCancelAffectedInput;
    pub use crate::runtime::TurnCancelAffectedWake;
    pub use crate::runtime::TurnCancelClosureAuthorization;
    pub use crate::runtime::TurnCancelClosureAuthorizationOutcome;
    pub use crate::runtime::TurnCancelClosureProposal;
    pub use crate::runtime::TurnCancelClosureSettlement;
    pub use crate::runtime::TurnCancelInputOutcome;
    pub use crate::runtime::TurnCancelIntentSnapshot;
    pub use crate::runtime::TurnCancelMode;
    pub use crate::runtime::TurnCancelOutcome;
    pub use crate::runtime::TurnCancelReceipt;
    pub use crate::runtime::TurnCancelRequest;
    pub use crate::runtime::TurnCancelRequestRecord;
    pub use crate::runtime::TurnCancelUndeliveredInputPolicy;
    pub use crate::runtime::TurnCancellationAuthority;
    pub use crate::runtime::TurnCancellationEvidence;
    pub use crate::runtime::TurnControlAttachment;
    pub use crate::runtime::TurnExecutionMetrics;
    pub use crate::runtime::TurnInputAcceptanceReceipt;
    pub use crate::runtime::TurnIssue;
    pub use crate::runtime::TurnIssueSeverity;
    pub use crate::runtime::TurnLaneAdmissionPolicy;
    pub use crate::runtime::TurnTerminal;
    pub use crate::runtime::TurnWorkDriver;
    pub use crate::runtime::UnreportedUsageAttempt;
    pub use crate::runtime::UsageReconciliationReport;
    pub use crate::runtime::UsageReportRow;
    pub use crate::runtime::UsageTotals;
    pub use crate::runtime::WakeDeliveryDriveReport;
    pub use crate::runtime::WakeDeliveryDriver;
    pub use crate::runtime::WatchedRegistry;
    pub use crate::runtime::WeakRuntimeHandle;
    pub use crate::runtime::await_event_identity;
    pub use crate::runtime::current_epoch_ms;
    pub use crate::runtime::diff_token_ledger;
    pub use crate::runtime::diff_usage_reports;
    pub use crate::runtime::effect::executor::control::facade_ops::ScopedEffectControllerFacadeOps;
    pub use crate::runtime::process_child_session_id;
    pub use crate::runtime::process_runtime_session_ids;
    pub use crate::runtime::process_signal_event_type;
    pub use crate::runtime::process_signal_wait_key;
    pub use crate::runtime::process_wake_delivery;
    pub use crate::runtime::process_wake_source_key;
    pub use crate::runtime::reconcile_pruned_trigger_deliveries;
    pub use crate::runtime::refuse_unhonored_group_membership;
    pub use crate::runtime::registry_transitions;
    pub use crate::runtime::release_bound_trigger_delivery_pins;
    pub use crate::runtime::turn_control_binding_id_for_scope;
    pub use crate::runtime::{ProcessChangeHub, ProcessChangeSubscription};
    pub use crate::runtime::{QueuedEffectSource, QueuedTurnOptions, TurnOptions};
    pub use crate::runtime::{SessionAdministration, SessionDeleteContext, SessionDeleteExecution};
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
    pub use crate::store::{CommitBudget, CommitBudgetLimit};
    pub use crate::tool_intent::legacy_tool_intent_v1_lookup_key;
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use crate::tool_registry::PLUGIN_TOOL_SOURCE_ID;
    pub use crate::tool_registry::ReconfigureError;
    pub use crate::tool_registry::SupersededToolIdentity;
    pub use crate::tool_registry::ToolRestoreReport;
    pub use crate::tool_registry::ToolSourceHandle;
    pub use crate::tool_registry::ToolSourcePolicy;
    pub use crate::tool_registry::ToolStateEntry;
    pub use crate::tool_registry::ToolSurfaceOpenMode;
    pub use crate::tool_registry::facade_ops::ToolRegistryFacadeOps;
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
    pub use crate::triggers::trigger_delivery_start_key;
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
    pub use lash_sansio::StreamMessageKind;
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
    pub use lash_sansio::tool_result_text;
    pub use lash_sansio::visible_response_text_from_parts;
    pub use lash_trace::JsonlTraceReadError;
    pub use lash_trace::JsonlTraceSink;
    pub use lash_trace::TraceBranchSelection;
    pub use lash_trace::TraceLabelMetadata;
    pub use lash_trace::TraceLevel;
    pub use lash_trace::TraceRecord;
    pub use lash_trace::TraceRuntimeScope;
    pub use lash_trace::TraceRuntimeSubject;
    pub use lash_trace::TraceSink;
    pub use lash_trace::TraceSinkError;
    pub use lash_trace::parse_jsonl_records;
    pub use schemars::JsonSchema;

    pub fn wake_delivery_driver_with_work_cadence(
        registry: std::sync::Arc<dyn crate::runtime::ProcessRegistry>,
        session_store_factory: std::sync::Arc<dyn crate::runtime::DeploymentStore>,
        queued_work: std::sync::Arc<dyn crate::runtime::SessionWorkEngine>,
        clock: std::sync::Arc<dyn crate::runtime::Clock>,
        delivery_policy: crate::runtime::DeliveryPolicy,
        work_cadence: crate::runtime::WorkCadencePolicy,
    ) -> Result<crate::runtime::WakeDeliveryDriver, crate::runtime::WorkCadenceError> {
        crate::runtime::WakeDeliveryDriver::with_work_cadence(
            registry,
            session_store_factory,
            queued_work,
            clock,
            delivery_policy,
            work_cadence,
        )
    }
}

pub(crate) use facade_support::*;

// `facade_support` is the workspace's internal cross-crate seam, and membership
// in it means some crate's *shipped* code needs the item (FIG-1223). These
// twelve had test-only consumers, so their public path is `test_support` and
// only their crate-internal short path lives here: `test_support` is
// feature-gated and `crate::X` has to resolve in every build.
pub(crate) use crate::attachments::{AttachmentProducer, AttachmentSourcePolicy};
pub(crate) use crate::plugin::RuntimeServices;

pub mod sansio {
    pub(crate) use lash_sansio::sansio::LogEvent;
    pub use lash_sansio::sansio::{
        ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
        ContextProjector, EffectId, ExecutionEnvironmentSync, ExpandedRow, ExpandedWrapper,
        LlmCallError, ModelToolCalls, PendingToolCall, PendingWork, ProjectorTurnInputs,
        ProtocolDriverHandle, Response, ResponseToolCalls, ToolExpansionPlan, TurnCause,
        TurnMachine, render_turn_causes_prompt,
    };
}

pub use attachments::{
    AttachmentGcFence, AttachmentReclamationPolicy, AttachmentRootSet, AttachmentStore,
    AttachmentStoreError, AttachmentStoreFailureClass, AttachmentStorePersistence,
    EmptyRootSetPolicy, StoredAttachment, StoredBlobRef,
};
pub use lash_core_execution::turn_outcome_from_tool_control;
pub use lash_sansio::llm::types::{
    AttachmentSource, AttemptOutcome, AttemptRecord, AttemptUsageOutcome, ChargeSafetyDecision,
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
    PromptContributionGate, PromptLayer, PromptSectionTitle, PromptSlot, PromptSlotLayer,
    PromptTemplate, PromptTemplateEntry, PromptTemplateSection, PromptTitleBuiltin, SchemaContract,
    SchemaProjectionOverride, SchemaProjectionPolicy, SessionAppendNode,
    TYPESCRIPT_TOOL_BINDING_KEY, TextProjectionMetadata, TokenUsage, TokenUsageOverflow,
    ToolArgumentProjectionPolicy, ToolBinding, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
    ToolCancellation, ToolCatalog, ToolCatalogBuildError, ToolCatalogEntry, ToolContract,
    ToolControl, ToolDefinition, ToolDefinitionBindingExt, ToolDiscovery, ToolFailure,
    ToolFailureClass, ToolFailureSource, ToolId, ToolIntentExecutionOutcome, ToolIntentIdentity,
    ToolIntentKind, ToolIntentRefusalReason, ToolManifest, ToolOutputContract, ToolRetryPolicy,
    ToolRetryStatus, ToolValue, ToolView, ToolViewBlock, ToolViewMeta, TurnCause, TurnId,
    TurnOutputSource,
};
pub(crate) use lash_sansio::{
    BaseRenderCache, PromptBuildInput, build_turn, messages_are_prompt_resume_safe,
    prompt_template_fingerprint, prompt_text_fingerprint, resolve_prompt_layers,
    visible_response_parts,
};
pub use protocol_build::ProtocolBuildInput;
pub use tool_provider::{ToolAttachmentClient, ToolDirectCompletionClient, ToolSessionModel};
pub use tool_registry::{
    SupersededToolIdentity, ToolRegistry, ToolRestoreReport, ToolSourcePolicy, ToolState,
    ToolSurfaceOpenMode,
};
pub use tool_result::{
    CancelHint, PendingAnnouncement, PendingCompletion, PendingResolver, TimeoutBehavior,
    ToolOutcome,
};
pub use tool_result::{DeclaredStart, DeclaredStartRefused};
pub use triggers::{
    TriggerCommand, TriggerCommandOutcome, TriggerDeliveryReservation,
    TriggerDeliveryReservationOutcome, TriggerDeliveryRetentionCandidate, TriggerEffectResult,
    TriggerEventCatalog, TriggerIngressReceipt, TriggerInputBinding, TriggerLifecycleColumnError,
    TriggerMutationOutcome, TriggerMutationReceipt, TriggerOccurrenceFilter,
    TriggerOccurrenceOutcome, TriggerOccurrenceReclamationReport,
    TriggerOccurrenceReclamationResult, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
    TriggerOperationError, TriggerOwnerScope, TriggerProviderRoute,
    TriggerRetentionReconciliationReport, TriggerRouteRefusal, TriggerRouteRestorer,
    TriggerSourceCapture, TriggerStore, TriggerSubscriptionDraft, TriggerSubscriptionFilter,
    TriggerSubscriptionLifecycle, TriggerSubscriptionRecord, admit_trigger_registration_target,
    trigger_handle_outcome_value,
};

pub(crate) mod facade_ops {}
pub use lash_core_execution::{
    ArtifactCarry, ArtifactCleanup, ArtifactCleanupPlan, ArtifactName, ArtifactReferrer,
    ArtifactReferrerError, ArtifactReferrerKind, ArtifactStoreId, AttachmentUploadId,
    DefinitionRevisionId, FrameEnvironmentId, HostArtifactPin, ReferrerClaim,
    ResolvedArtifactCleanup, RuntimeOwner, SubscriptionRevisionId, UploadReferrerId,
    artifact_referrer_ended, trigger_incarnation,
};
pub use lash_core_execution::{
    ArtifactPublicationPause, ArtifactStoreError, Backend, DurabilityTier, EffectEngine,
    ModuleArtifactStore, StoreBindingId, StoreSet,
};
pub use lash_core_execution::{
    DriverAction, DriverContextView, Effect, HostTurnProtocol, PreparedTurnMachine,
    ProjectorContext, ProtocolDriverState, SansIoTurnInput, TurnDriverConfig, TurnDriverPreamble,
    TurnMachine, TurnMachineConfig,
};
pub use lash_core_execution::{
    ParentEndApplication, apply_parent_end_plan, end_parent_scope, end_session_roots,
    parent_end_delivery_key, parent_end_requester,
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
pub(crate) use plugin::PluginRuntimeDirective;
pub use plugin::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, AppendSessionNodesOutcome,
    AppendSessionNodesRequest, FrameNodeId, FrameNodeIdError, KeyRejection, PluginError,
    PluginExtensions, PluginNamespaceState, PluginOptions, PluginState, PluginStateEdit,
    PluginStateError, PluginStateStore, ProcessEngineContributionContext,
    ProtocolBeforeLlmCallContext, ProtocolLlmCallAction, SessionCreateRequest, SessionGraphService,
    SessionLineage, SessionPluginInit, SessionPluginSource, SessionReadView, SessionRelation,
    SessionSnapshot, SessionStartPoint, SessionStateService, SessionToolAccess,
    SessionToolAccessError, SubagentSessionContext, durable_identity_conflict,
    is_durable_identity_conflict,
};
pub use plugin::{OpenAgentFrameRequest, OpenAgentFrameResult};

pub use provider::{
    AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
    AttachmentCapabilitySnapshot, AttachmentMimeSource, CacheControlDialect, GoogleDialect,
    InstructionRole, ModelCapability, OpenAiReasoningContext, ReasoningCapability,
    ReasoningEncoding, ReasoningIntent, ReasoningRetentionCapability, ReasoningRetentionPolicy,
    ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
    ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability, StreamTermination,
};
pub(crate) use provider::{ProviderCompletion, ProviderCompletionError, RuntimeProviderResolver};
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
pub use runtime::fail_parent_end_once;
pub use runtime::{ObservationSource, drive_with_observations};

// This block includes the effect / process-control types consumed by external
// effect hosts (e.g. lash-restate's workflows) and their integration tests —
// they are deliberately public; the rest of the runtime module stays
// crate-internal.
/// Intent realization publishes the execution environment a declared trigger
/// subscription names, under the realizing execution's journal referrer
/// (FIG-3116, ADR 0113 §3.4).
pub use lash_core_execution::runtime::publish_process_execution_env;
/// The artifact ports a process-engine registry acquires start and revision
/// artifacts through (ADR 0113 §3.3), for hosts that assemble a registry
/// outside `RuntimeHostConfig`.
pub use lash_core_execution::runtime::{ArtifactReferrerPorts, ReferrerAcquisition};
pub use process_registry::{
    ProcessDefinitionExpectation, ProcessDefinitionLifecycle, ProcessDefinitionRecord,
    ProcessDefinitionRegistration, ProcessDefinitionRegistry,
};
pub use runtime::{
    AbandonEvidence, AbandonWriter, ActiveTurnIngress, AdmittedProcessIdentity, AdmittedScope,
    AdmittedTurnInputs, Ancestry, AssistantResponseHookEvents, AssistantStreamHookState,
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BindingId, BoundaryReason,
    CapabilityRef, CausalRef, ChargeSafetyRefusalEvidence, CheckpointAdmittedSet, Clock,
    ClockWallTime, CommandJournalGuard, CommandReplayKey, CompletionKeyPreparation, ContractRef,
    DeclaredProcessIdentity, DefinitionRef, DeliveryPolicy, DeploymentStore,
    DeploymentStoreDecorator, DrainMode, DrainModePolicy, EffectAddress, EffectGroupDrainBudget,
    EffectGroupHandle, EffectGroupMembership, EffectHost, EffectJournalRetirement, EffectOpener,
    EffectOpenerError, EffectRetirementGate, ExecutableGeneration, ExecutableGenerationRefusal,
    ExecutionScope, ForkPoint, ForkSessionReceipt, ForkSessionRequest, GroupChildBinding,
    GroupChildCancelWatch, GroupExecutors, GroupReopen, GroupSettlement, GroupWakePolicy, HandleId,
    InputItem, InvalidProcessDefinitionId, InvalidStartKey, JournalReplay, LedgerUsageOutcome,
    Lifetime, LifetimeDecision, LifetimePolicy, LiveReplayEventDraft, LiveReplayGapReason,
    LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeOutcome,
    LiveReplaySubscription, LlmRequestSpec, LlmStreamRecord, LocalTurnStop, LoserPolicy,
    MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NoProcessWork, NoSessionWork, NonTerminalProcessPage,
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION,
    PROCESS_WAKE_DELIVERY_FORMAT_VERSION, PROCESS_WAKE_MERGE_KEY, ParentEndPlan, PendingTurnInput,
    PendingTurnInputBatch, PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt,
    PendingTurnInputCancelTarget, PendingTurnInputDraft, PendingTurnInputRead,
    PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome, PersistedSegmentHandover,
    PreparedLiveReplayPublication, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessClockRebind, ProcessCommand, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessDefinition, ProcessDefinitionDraft,
    ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionTarget,
    ProcessDefinitionValue, ProcessDriveStep, ProcessEffectNodeReport, ProcessEffectOccurrence,
    ProcessEffectOmissions, ProcessEffectOmittedCounts, ProcessEffectOutcome,
    ProcessEffectOutcomeClass, ProcessEffectReport, ProcessEffectReportError, ProcessEngine,
    ProcessEngineAdmission, ProcessEngineKind, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleView, ProcessId, ProcessIdMint, ProcessIdentity,
    ProcessInfraError, ProcessInput, ProcessLifecycle, ProcessLineage, ProcessListFilter,
    ProcessListMode, ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry,
    ProcessOpScope, ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome,
    ProcessOutcomeObserver, ProcessProvenance, ProcessPruneReport, ProcessQuery, ProcessRecord,
    ProcessRegistrar, ProcessRegistration, ProcessRegistrationOutcome, ProcessRegistrationProbe,
    ProcessRegistrationReceipt, ProcessRegistry, ProcessRegistryAwaiter, ProcessRegistryBinding,
    ProcessRegistryCursor, ProcessResumeRefusal, ProcessRetention, ProcessRunOutcome,
    ProcessScopeFenceHosts, ProcessSegmentKey, ProcessService, ProcessSessionDeleteReport,
    ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartReceipt, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTerminalPublication, ProcessTerminalSpec, ProcessTerminalWait,
    ProcessTombstone, ProcessToolIntents, ProcessValueSelector, ProcessWakeDelivery,
    ProcessWakeOutbox, ProcessWakeSpec, ProcessWorkSubstrate, ProcessWorkWiring,
    ProjectionWatermark, ProtocolSessionExtension, ProtocolSessionExtensionHandle,
    QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection,
    QueuedWorkAuthority, QueuedWorkBatchingConfig, QueuedWorkKind, RankedGroupSettlement,
    RecordedJournal, RecordedKeyFence, RecordedKeyRange, RecordedKeys, RecordedRender,
    RefusedWriteRange, RegistryScopeClose, Resolution, ResolveOutcome, ResolvedRun, RunDefinition,
    RunDefinitions, RunOverrides, RunShapeError, RunSpec, RunSpecHash, RuntimeAttribution,
    RuntimeCheckpointComponents, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchReport, RuntimeError, RuntimeErrorCause, RuntimeErrorCode,
    RuntimeInvocation, RuntimeReplay, RuntimeReplayAttribution, RuntimeSessionState,
    SCOPE_STORAGE_PAYLOAD_VERSION, ScopeBoundController, ScopeGrant, ScopeId, ScopeRef,
    ScopeStorageError, ScopedEffectController, SegmentHandover, SegmentProgress,
    SegmentStartMarker, ServedOnly, ServedOnlyRange, SessionAdministration, SessionCreationHead,
    SessionCursor, SessionCursorError, SessionDeleteContext, SessionDeleteExecution, SessionDriver,
    SessionId, SessionListFilter, SessionObservationEvent, SessionObservationEventPayload,
    SessionProcessEventKind, SessionQueueEventKind, SessionRelationKind, SessionRevision,
    SessionScope, SessionStateVersionRefusal, SessionStoreCreateRequest, SessionUsageTotals,
    SessionView, SessionWorkEngine, SleepSpec, SlotId, StartCx, StartCxError, StartKey,
    StoreRealization, TokenLedgerEntry, ToolAttemptLaunch, ToolIntentOutcomeSink,
    ToolIntentPreparation, ToolIntentSubmissionGuard, TurnActivity, TurnActivityId,
    TurnCancelAffectedInput, TurnCancelAffectedWake, TurnCancelClosureAuthorization,
    TurnCancelClosureAuthorizationOutcome, TurnCancelClosureOwnerBinding,
    TurnCancelClosureProposal, TurnCancelClosureSettlement, TurnCancelGatePair,
    TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode, TurnCancelRequestRecord,
    TurnCancelUndeliveredInputPolicy, TurnCancelWait, TurnCancellationAuthority, TurnContext,
    TurnControlAttachment, TurnControlBinding, TurnControlBindingId, TurnControlBindingIdError,
    TurnEvent, TurnFailureCause, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement, TurnInput, TurnInputAdmissionMode, TurnInputApplication,
    TurnInputCheckpointBoundary, TurnInputCompletion, TurnInputCompletionData, TurnInputIngress,
    TurnInputState, TurnInputStateKind, TurnLaneAdmissionPolicy, UnreportedLedgerAttempt,
    UsageOutcomeError, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryLifecycle, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WatchedRegistry, WeakProcessEngineRegistry,
    WorkCadenceError, WorkCadencePolicy, admit_session_state_generation,
    artifact_store_plugin_error, effect_groups_unsupported, lifetime, mint_process_id,
    park_turn_of_refused_group_child, park_turn_refused_by_generation, retry_cancel_watch,
    tool_failure_code,
};
#[allow(unused_imports)]
pub(crate) use runtime::{
    AdmissionBoundary, AdmittedQueuedWork, ProcessEventSemantics, QueuedCheckpointTurnInput,
    QueuedCheckpointWork, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkCompletion,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkPayload, RuntimeSubject, TurnWorkPayload,
    load_process_execution_env, materialize_process_event_semantics, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_event_invocation, process_wake_batch_draft, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text, require_event_replay,
};
pub use runtime::{ConsumerHold, PinnedTriggerDelivery, SessionTurnOutcome, TriggerDeliveryPin};
pub(crate) use runtime::{ProcessEngineRunGuard, ProcessEngineRuntimeContext};
pub(crate) use session_model::plugin_runtime_protocol_event;

pub(crate) use session::RuntimeExecutionProcessEventContext;
pub(crate) use session::RuntimeExecutionTracing;
pub(crate) use session::Session;
pub use session::{
    ExecRequest, PluginOptionsUnaccepted, RuntimeExecutionContext, SessionConfigRefusal,
    SessionError, ToolDispatchSurface, ToolSurfaceDrift, ToolSurfaceDriftKind,
    resolve_trigger_owner_scope, tool_dispatch_surface,
};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SESSION_NODE_BODY_SCHEMA_VERSION, SessionGraph,
    SessionNodePayload, SessionNodeRecord,
};

pub use session_model::{ChargeSafetyPolicy, NoProgressBudget, SessionPolicy, TurnBudget};
pub use session_model::{ProtocolEvent, SessionHistoryRecord};
pub use store::{
    AdmissionRefusal, AdoptedAttachmentCondemnation, AppendRequestIdentity, AttachmentCondemnation,
    AttachmentCondemnationAdoption, AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
    AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentDeleteArming,
    AttachmentDeleteStallReason, AttachmentManifest, AttachmentSettlementOutcome,
    AttachmentSweepGeneration, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit,
    AttachmentWriteToken, BlobRef, CURRENT_SESSION_STATE_VERSION, CheckpointComponentDescriptor,
    CommitBudget, CommitBudgetLimit, DurableItem, DurablePayload, DurableScan, DurableScanPage,
    DurableSurface, FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState, FleetFormatStore,
    GcReport, HydratedCheckpointComponent, HydratedSessionCheckpoint, LeaseOwnerIdentity,
    MAX_ATTACHMENT_DELETE_ATTEMPTS, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport,
    MaintenanceResult, MaintenanceStop, MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION,
    OperationId, QueuedWorkStore, RetentionBound, RetentionReport, RuntimeCommit, RuntimeStore,
    RuntimeStoreDecorator, RuntimeTurnCommitStamp, RuntimeUsageDelta, RuntimeUsageDeltaIdentity,
    ScanCoverage, SemanticBoundaryOperation, SessionAdmission, SessionBinding,
    SessionBlobReclaimReport, SessionCatalogStore, SessionCommitStore, SessionHistoryStore,
    SessionLookup, SessionMeta, SessionReferrerState, SessionStateAdmission, SessionStore,
    StoreBackend, StoreComponentVersion, StoreError, StoreMaintenance, StorePreflight,
    StoreReleaseStamp, StoreReleaseState, StoreSchemaDatabase, StoreSchemaOutcome,
    StoreSchemaStatus, StoreSchemaVerdict, SurfaceFormat, TurnInputAdmission, TurnInputStore,
    VacuumReport, WriterPin, compare_releases, release_stamp_advances,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphAppend, RuntimeCommitReceipt, SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
    ensure_supported_schema_version,
};
pub use tool_intent::{
    CancelProcessIntent, DeclaredModuleArtifact, EmitProcessEventIntent, EmitTriggerIntent,
    RegisterProcessDefinitionIntent, RegisterTriggerIntent, SignalProcessIntent,
    StartProcessIntent, TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT,
    TOOL_INTENT_MAX_PER_KIND, TOOL_INTENT_PROTOCOL_V3, ToolAttemptOutcome, ToolIntent,
    ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord, ToolIntents, ToolOutcomeDone,
    derive_tool_intent_identity, derive_tool_intent_identity_under, rederive_tool_intent_identity,
};
/// Tool-provider contracts, including child-process execution observation hooks.
pub use tool_provider::{
    AttemptContext, AttemptProcessReads, AttemptSessionReads, PreparedToolBatch,
    PreparedToolBatchCall, PreparedToolCall, ProcessToolCall, ToolCall,
    ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolExecutionGrant, ToolPrepareCall,
    ToolPrepareContext, ToolProvider,
};
#[doc(hidden)]
pub mod core_internal {
    pub use crate::runtime::RuntimeSessionServices;
    pub use lash_core_execution::core_internal::{
        StartKeyDerivation, attach_process_invocation_correlation,
        clear_process_invocation_correlation,
    };
}

#[cfg(test)]
mod attachments_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_agent_frame_seed_is_rejected_at_the_serde_boundary() {
        let frame_key =
            FrameKey::from_caller_material("delegate").expect("non-empty caller material");
        let err = serde_json::from_value::<ToolControl>(serde_json::json!({
            "type": "switch_agent_frame",
            "frame_key": frame_key,
            "initial_nodes": [{ "not": "a session append node" }],
            "task": "continue the work"
        }))
        .expect_err("invalid seed cannot construct a tool control");

        assert!(err.to_string().contains("kind"), "unexpected error: {err}");
    }
}
