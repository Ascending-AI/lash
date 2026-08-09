//! Runtime kernel for Lash.
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

pub mod attachments;
pub mod chronological;
pub mod direct;
mod identity_json;
pub mod llm;
mod model;
pub mod panic_containment;
pub mod plugin;
mod plugin_stack;
mod protocol_build;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod session_graph;
pub(crate) mod session_graph_integrity;
pub mod session_model;
mod stable_hash;
mod stable_identity;
pub mod store;
pub mod task;
/// Standard-lock poison recovery traits used across Lash hosts and runtimes.
pub mod sync {
    pub use lash_sansio::sync::*;
}
#[cfg(test)]
mod test_watchdog;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tool_dispatch;
mod tool_provider;
pub mod tool_registry;
mod tool_result;
mod trace;
pub mod triggers;

#[doc(hidden)]
pub mod store_backend_support {
    pub use crate::runtime::turn_input_ingress::derive_pending_turn_input_id;
    pub use crate::store::session_execution_lease::{
        SessionExecutionLeaseFenceFacts, SessionExecutionLeaseRefusalFacts,
        SessionExecutionLeaseRefusalOperation, require_current_session_execution_lease,
        trace_session_execution_lease_refusal,
    };
}

#[doc(hidden)]
pub mod facade_support {
    /// Build the core-level tool-registry projection through the same plugin
    /// composition path used for runtime sessions.
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

    pub use crate::attachments::AttachmentProducer;
    pub use crate::attachments::AttachmentReclamationReport;
    pub use crate::attachments::AttachmentSourcePolicy;
    pub use crate::attachments::AttachmentSourcePolicyError;
    pub use crate::attachments::FileAttachmentStore;
    pub use crate::attachments::InMemoryAttachmentStore;
    pub use crate::attachments::OpenAttachmentSourcePolicy;
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
    pub use crate::direct::DirectLlmResult;
    pub use crate::direct::DirectMessage;
    pub use crate::direct::DirectOutputSpec;
    pub use crate::direct::DirectPart;
    pub use crate::direct::DirectRequest;
    pub use crate::direct::DirectRole;
    pub use crate::facade_ops::ProtocolTurnOptionsFacadeOps;
    pub use crate::llm::transport::LlmTransportError;
    pub use crate::llm::transport::ProviderFailure;
    pub use crate::plugin::AssistantResponseTransform;
    pub use crate::plugin::CheckpointHookContext;
    pub use crate::plugin::CompactionContext;
    pub use crate::plugin::ContextCompaction;
    pub use crate::plugin::ContextCompactor;
    pub use crate::plugin::ContextError;
    pub use crate::plugin::DirectCompletion;
    pub use crate::plugin::DirectLlmCompletion;
    pub use crate::plugin::PersistentRuntimeServices;
    pub use crate::plugin::PluginCommand;
    pub use crate::plugin::PluginCommandReceipt;
    pub use crate::plugin::PluginDirective;
    pub use crate::plugin::PluginExtensionContribution;
    pub use crate::plugin::PluginFactory;
    pub use crate::plugin::PluginHost;
    pub use crate::plugin::PluginLifecycleEvent;
    pub use crate::plugin::PluginLifecycleEventHook;
    pub use crate::plugin::PluginOperation;
    pub use crate::plugin::PluginOperationFailure;
    pub use crate::plugin::PluginOperationInvokeError;
    pub use crate::plugin::PluginOwned;
    pub use crate::plugin::PluginQuery;
    pub use crate::plugin::PluginRegistrar;
    pub use crate::plugin::PluginSession;
    pub use crate::plugin::PluginSessionContext;
    pub use crate::plugin::PluginSpec;
    pub use crate::plugin::PluginSpecFactory;
    pub use crate::plugin::PluginTask;
    pub use crate::plugin::PluginTaskReceipt;
    pub use crate::plugin::PromptHookContext;
    pub use crate::plugin::RuntimeServices;
    pub use crate::plugin::SessionConfigChangedContext;
    pub use crate::plugin::SessionHandle;
    pub use crate::plugin::SessionLifecycleService;
    pub use crate::plugin::SessionObservedProcessOutcome;
    pub use crate::plugin::SessionObservedProcessResult;
    pub use crate::plugin::SessionParam;
    pub use crate::plugin::SessionPlugin;
    pub use crate::plugin::SessionStateChangedContext;
    pub use crate::plugin::SessionTurnRequest;
    pub use crate::plugin::SnapshotReader;
    pub use crate::plugin::SnapshotWriter;
    pub use crate::plugin::ToolCatalogContribution;
    pub use crate::plugin::ToolResultProjectionContext;
    pub use crate::plugin::ToolResultProjector;
    pub use crate::plugin::TurnContextTransform;
    pub use crate::plugin::TurnHookContext;
    pub use crate::plugin::TurnResultHookContext;
    pub use crate::plugin::TurnResultSummary;
    pub use crate::plugin::TurnTransformContext;
    pub use crate::plugin::session_types::facade_ops::AgentFrameReasonFacadeOps;
    pub use crate::plugin_stack::PluginStack;
    pub use crate::provider::CacheRetention;
    pub use crate::provider::LlmTimeouts;
    pub use crate::provider::ModelEffortValidationCategory;
    pub use crate::provider::Provider;
    pub use crate::provider::ProviderComponents;
    pub use crate::provider::ProviderFactory;
    pub use crate::provider::ProviderHandle;
    pub use crate::provider::ProviderOptions;
    pub use crate::provider::ProviderSpec;
    pub use crate::provider::SingleProviderResolver;
    pub use crate::runtime::AgentFrameRun;
    pub use crate::runtime::AssembledTurn;
    pub use crate::runtime::AssistantOutput;
    pub use crate::runtime::CanonicalRuntimeEffectEnvelope;
    pub use crate::runtime::DEFAULT_PROCESS_EXECUTION_CONCURRENCY;
    pub use crate::runtime::DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY;
    pub use crate::runtime::DirectCompletionClient;
    pub use crate::runtime::DurableProcessWorker;
    pub use crate::runtime::DurableProcessWorkerConfig;
    pub use crate::runtime::EmbeddedRuntimeHost;
    pub use crate::runtime::EventSink;
    pub use crate::runtime::ExecutionSummary;
    pub use crate::runtime::InMemoryLiveReplayStore;
    pub use crate::runtime::InMemoryLiveReplayStoreConfig;
    pub use crate::runtime::InMemoryProcessExecutionEnvStore;
    pub use crate::runtime::InMemorySessionStore;
    pub use crate::runtime::InMemorySessionStoreFactory;
    pub use crate::runtime::InlineEffectHost;
    pub use crate::runtime::InlineProcessRunHandle;
    pub use crate::runtime::InlineRuntimeEffectController;
    pub use crate::runtime::LashRuntime;
    pub use crate::runtime::LiveReplayGap;
    pub use crate::runtime::MergeKey;
    pub use crate::runtime::NoopTurnActivitySink;
    pub use crate::runtime::ObservedProcess;
    pub use crate::runtime::ObservedProcessEvent;
    pub use crate::runtime::ObservedWorkItem;
    pub use crate::runtime::OutputState;
    pub use crate::runtime::PROCESS_LEASE_SCHEMA_VERSION;
    pub use crate::runtime::ParkedSession;
    pub use crate::runtime::ProcessAttach;
    pub use crate::runtime::ProcessAwaiter;
    pub use crate::runtime::ProcessChangeHub;
    pub use crate::runtime::ProcessDrainDeferred;
    pub use crate::runtime::ProcessDrainReport;
    pub use crate::runtime::ProcessEngineProcessContext;
    pub use crate::runtime::ProcessEngineRegistry;
    pub use crate::runtime::ProcessEventAppendPlan;
    pub use crate::runtime::ProcessEventSink;
    pub use crate::runtime::ProcessExecutionConcurrencyError;
    pub use crate::runtime::ProcessRecoveryAttemptDisposition;
    pub use crate::runtime::ProcessRecoveryOperation;
    pub use crate::runtime::ProcessRunHandle;
    pub use crate::runtime::ProcessRuntimeHost;
    pub use crate::runtime::ProcessStartPlan;
    pub use crate::runtime::ProcessTerminalSemantics;
    pub use crate::runtime::ProcessToolVisibilityFilter;
    pub use crate::runtime::ProcessTurnCancellation;
    pub use crate::runtime::ProcessWake;
    pub use crate::runtime::ProcessWakeDeliveryRequest;
    pub use crate::runtime::ProcessWorkDriver;
    pub use crate::runtime::ProcessWorkObserver;
    pub use crate::runtime::ProcessWorkSnapshot;
    pub use crate::runtime::QUEUED_WORK_SLOW_WAKE_THRESHOLD;
    pub use crate::runtime::QueuedWorkDriver;
    pub use crate::runtime::QueuedWorkExecutionConcurrencyError;
    pub use crate::runtime::QueuedWorkRunError;
    pub use crate::runtime::QueuedWorkRunHandle;
    pub use crate::runtime::QueuedWorkRunProgress;
    pub use crate::runtime::QueuedWorkRunRequest;
    pub use crate::runtime::QueuedWorkSlowWake;
    pub use crate::runtime::QueuedWorkWakeContended;
    pub use crate::runtime::QueuedWorkWakeDisposition;
    pub use crate::runtime::QueuedWorkWakeFailure;
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
    pub use crate::runtime::TurnCancelOutcome;
    pub use crate::runtime::TurnCancelReceipt;
    pub use crate::runtime::TurnCancelRequest;
    pub use crate::runtime::TurnCancellationEvidence;
    pub use crate::runtime::TurnInputAcceptanceReceipt;
    pub use crate::runtime::TurnIssue;
    pub use crate::runtime::TurnOptions;
    pub use crate::runtime::TurnTerminal;
    pub use crate::runtime::TurnWorkDriver;
    pub use crate::runtime::UnavailableProcessService;
    pub use crate::runtime::UsageReportRow;
    pub use crate::runtime::UsageTotals;
    pub use crate::runtime::WakeCoalescingKey;
    pub use crate::runtime::WakeDeliveryDriveReport;
    pub use crate::runtime::WakeDeliveryDriver;
    pub use crate::runtime::WakeTurnMode;
    pub use crate::runtime::WakeTurnPolicy;
    #[doc(hidden)]
    pub use crate::runtime::await_event_coordinator;
    pub use crate::runtime::current_epoch_ms;
    pub use crate::runtime::diff_token_ledger;
    pub use crate::runtime::diff_usage_reports;
    pub use crate::runtime::effect::executor::control::facade_ops::ScopedEffectControllerFacadeOps;
    #[doc(hidden)]
    pub use crate::runtime::effect_replay_driver;
    pub use crate::runtime::ensure_durable_effect_input;
    pub use crate::runtime::epoch_ms_from_system_time;
    pub use crate::runtime::facade_ops::TurnContextFacadeOps;
    pub use crate::runtime::process_runtime_session_ids;
    pub use crate::runtime::process_signal_event_type;
    pub use crate::runtime::process_signal_wait_key;
    pub use crate::runtime::process_wake_delivery;
    pub use crate::runtime::process_wake_source_key;
    pub use crate::runtime::promise_semantics;
    pub use crate::runtime::reconcile_pruned_trigger_deliveries;
    #[doc(hidden)]
    pub use crate::runtime::registry_transitions;
    pub use crate::runtime::state::facade_ops::RuntimeSessionStateFacadeOps;
    pub use crate::runtime::system_time_from_epoch_ms;
    pub use crate::runtime::terminal_append_request;
    pub use crate::runtime::validate_generic_process_event_append;
    pub use crate::runtime::validate_replayed_effect_envelope;
    pub use crate::runtime::watch_process_registry;
    pub use crate::runtime::watch_process_registry_with_sink;
    pub use crate::runtime::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};
    pub use crate::session::InjectedTurnInput;
    pub use crate::session::ToolInvocation;
    pub use crate::session::ToolInvocationReply;
    pub use crate::session_graph::facade_ops::{SessionGraphFacadeOps, SessionNodeRecordFacadeOps};
    pub use crate::session_graph::frame_node_id;
    pub use crate::session_model::ConversationRecord;
    pub use crate::session_model::GenerationOverlay;
    pub use crate::session_model::SessionSpec;
    pub use crate::session_model::context::PreparedContext;
    pub use crate::store::LeaseTimings;
    pub use crate::store::LeaseTimingsError;
    pub use crate::store::SessionHead;
    pub use crate::store::SessionPickerInfo;
    pub use crate::store::{CommitBudget, CommitBudgetLimit};
    pub use crate::tool_provider::ToolChildExecutionTraceHook;
    pub use crate::tool_provider::ToolSessionProcessAdmin;
    pub use crate::tool_provider::ToolTriggerClient;
    pub use crate::tool_registry::PLUGIN_TOOL_SOURCE_ID;
    pub use crate::tool_registry::ReconfigureError;
    pub use crate::tool_registry::ToolRestoreReport;
    pub use crate::tool_registry::ToolSourceHandle;
    pub use crate::tool_registry::ToolStateEntry;
    pub use crate::tool_registry::facade_ops::{ToolRegistryFacadeOps, ToolStateFacadeOps};
    pub use crate::triggers::InMemoryTriggerStore;
    pub use crate::triggers::TriggerDeliveryEmitOutcome;
    pub use crate::triggers::TriggerDeliveryEmitReport;
    pub use crate::triggers::TriggerEmitReport;
    pub use crate::triggers::TriggerEvent;
    pub use crate::triggers::TriggerEventType;
    pub use crate::triggers::TriggerManifestMembership;
    pub use crate::triggers::TriggerRegistration;
    pub use crate::triggers::TriggerRouter;
    pub use crate::triggers::TriggerTargetSummary;
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
    pub use lash_sansio::AcceptedInjectedTurnInput;
    pub use lash_sansio::AttachmentMeta;
    pub use lash_sansio::EffectId;
    pub use lash_sansio::ErrorEnvelope;
    pub use lash_sansio::MessageSequence;
    pub use lash_sansio::ModelToolReturn;
    pub use lash_sansio::ModelToolReturnPart;
    pub use lash_sansio::ProviderSchemaCapabilities;
    pub use lash_sansio::ResolvedSchema;
    pub use lash_sansio::Response;
    pub use lash_sansio::SchemaDialect;
    pub use lash_sansio::SchemaPurpose;
    pub use lash_sansio::SchemaResolutionError;
    pub use lash_sansio::SchemaResolutionRequest;
    pub use lash_sansio::SessionStreamEvent;
    pub use lash_sansio::ToolCatalogBuildInput;
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
    pub use lash_sansio::validate_tool_input;
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

pub mod sansio {
    pub(crate) use lash_sansio::sansio::LogEvent;
    pub use lash_sansio::sansio::{
        ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
        ContextProjector, EffectId, ExecutionEnvironmentSync, LlmCallError, PendingToolCall,
        ProtocolDriverHandle, Response, TurnCause, TurnMachine, WaitingExecState, WaitingLlmState,
        render_turn_causes_prompt,
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Which layer owns replay of runtime effects.
///
/// This is a mechanical property of an effect controller, not an end-to-end
/// durability claim. Controller-owned replay means a controller journals or
/// otherwise replays completed effects; runtime-owned execution means Lash
/// invokes the local effect path directly on each turn attempt.
pub enum EffectReplayOwnership {
    #[default]
    Runtime,
    Controller,
}

// Re-exports
pub use attachments::{
    AttachmentRootSet, AttachmentStore, AttachmentStoreError, AttachmentStorePersistence,
    StoredAttachment, StoredBlobRef,
};
pub use lash_sansio::llm::types::{
    AttachmentSource, AttemptOutcome, AttemptRecord, ExecutionEvidence, GenerationDisposition,
    GenerationOptionDisposition, GenerationOptions, LlmCallId, LlmCallRecord, LlmOutputPart,
    LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalReason, NonNegativeFiniteF64,
    NormalizedError, ProtocolPosition, ProviderFileScope, RetryDecision,
};
pub use lash_sansio::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, CheckpointDelivery,
    CheckpointKind, CompactToolContract, ExecImage, ExecResponse, ExecutedCallOutcome,
    ExecutedCallRecord, LashSchema, LlmCallError, MediaType, Message, MessageOrigin, MessageRole,
    Part, PartKind, PluginMessage, PluginRuntimeEvent, ProjectionMode, PromptBuiltin,
    PromptContribution, PromptContributionGate, PromptLayer, PromptSlot, PromptSlotLayer,
    PromptTemplate, PromptTemplateEntry, PromptTemplateSection, PruneState, SchemaContract,
    SchemaProjectionOverride, SchemaProjectionPolicy, SessionAppendNode, TextProjectionMetadata,
    TokenUsage, TokenUsageOverflow, ToolActivation, ToolArgumentProjectionPolicy, ToolCallOutcome,
    ToolCallOutput, ToolCallRecord, ToolCancellation, ToolCatalog, ToolCatalogEntry, ToolContract,
    ToolControl, ToolDefinition, ToolFailure, ToolFailureClass, ToolFailureSource, ToolId,
    ToolManifest, ToolOutputContract, ToolRetryDisposition, ToolRetryPolicy, ToolValue, TurnCause,
};
pub(crate) use lash_sansio::{
    BaseRenderCache, PromptBuildInput, build_turn, messages_are_prompt_resume_safe,
    prompt_template_fingerprint, prompt_text_fingerprint, resolve_prompt_layers,
    visible_response_parts,
};
pub use store::AttachmentOwnerKind;

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
            frame_id,
            initial_nodes,
            task: Some(task),
        } if !frame_id.trim().is_empty() && !task.trim().is_empty() => {
            Some(TurnOutcome::AgentFrameSwitch {
                frame_id: frame_id.clone(),
                task: task.clone(),
                initial_nodes: initial_nodes.clone(),
            })
        }
        ToolControl::Finish { value } => Some(TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: tool_name.to_string(),
            value: value.to_json_value(),
        })),
        ToolControl::Fail { failure } => Some(TurnOutcome::Stopped(TurnStop::ToolError {
            tool_name: tool_name.to_string(),
            value: failure.to_json_value(),
        })),
        ToolControl::SwitchAgentFrame { .. } => None,
    }
}
pub use protocol_build::ProtocolBuildInput;
pub use tool_registry::{ToolRegistry, ToolState};
pub use tool_result::{CancelHint, PendingCompletion, TimeoutBehavior, ToolResult};
pub use triggers::{
    TriggerCommand, TriggerCommandOutcome, TriggerDeliveryReservation,
    TriggerDeliveryReservationStatus, TriggerDeliveryRetentionCandidate, TriggerEffectResult,
    TriggerEventCatalog, TriggerIngressResult, TriggerInputBinding, TriggerMutationDisposition,
    TriggerMutationReceipt, TriggerOccurrenceFilter, TriggerOccurrenceRecord,
    TriggerOccurrenceRequest, TriggerOperationError, TriggerOwnerScope, TriggerStore,
    TriggerSubscriptionDraft, TriggerSubscriptionFilter, TriggerSubscriptionRecord,
};
pub(crate) const PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProtocolTurnOptions {
    pub schema_version: u32,
    pub payload: serde_json::Value,
}

fn empty_protocol_turn_payload() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolTurnOptionsError {
    #[error(
        "protocol turn options are missing schema_version and were written by unsupported pre-versioned state (expected {expected})"
    )]
    MissingSchemaVersion { expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is not supported by this binary (expected {expected})"
    )]
    UnsupportedSchemaVersion { actual: u32, expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is invalid (expected integer {expected})"
    )]
    InvalidSchemaVersion { actual: String, expected: u32 },
    #[error("failed to decode protocol turn options payload: {0}")]
    Decode(#[source] serde_json::Error),
}

fn parse_protocol_turn_options_schema_version(
    value: Option<serde_json::Value>,
) -> Result<u32, ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    let Some(value) = value else {
        return Err(ProtocolTurnOptionsError::MissingSchemaVersion { expected });
    };
    let Some(actual) = value
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
    else {
        return Err(ProtocolTurnOptionsError::InvalidSchemaVersion {
            actual: value.to_string(),
            expected,
        });
    };
    ensure_protocol_turn_options_schema_version(actual)?;
    Ok(actual)
}

fn ensure_protocol_turn_options_schema_version(
    actual: u32,
) -> Result<(), ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolTurnOptionsError::UnsupportedSchemaVersion { actual, expected })
    }
}

impl<'de> serde::Deserialize<'de> for ProtocolTurnOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ProtocolTurnOptionsWire {
            schema_version: Option<serde_json::Value>,
            #[serde(default = "empty_protocol_turn_payload")]
            payload: serde_json::Value,
        }

        let wire = ProtocolTurnOptionsWire::deserialize(deserializer)?;
        let schema_version = parse_protocol_turn_options_schema_version(wire.schema_version)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema_version,
            payload: wire.payload,
        })
    }
}

impl Default for ProtocolTurnOptions {
    fn default() -> Self {
        Self::empty()
    }
}

impl ProtocolTurnOptions {
    /// Constructs schema-current empty object options for protocol implementors materializing a
    /// turn with no protocol-specific overrides.
    pub fn empty() -> Self {
        Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Wraps an arbitrary JSON payload at the current schema version for protocol implementors
    /// materializing turn-specific state.
    pub fn from_payload(payload: serde_json::Value) -> Self {
        Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload,
        }
    }

    /// Reports empty only for an empty JSON object so protocol implementors do not confuse scalar,
    /// list, or null payloads with absent options.
    pub fn is_empty(&self) -> bool {
        match &self.payload {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    /// Serializes typed protocol options at the current schema version for protocol implementors
    /// materializing a turn.
    pub fn typed<T>(value: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload: serde_json::to_value(value)?,
        })
    }
}

impl ProtocolTurnOptions {
    /// Checks the schema version before deserializing typed options for protocol implementors,
    /// preserving version and payload errors separately.
    pub fn decode<T>(&self) -> Result<T, ProtocolTurnOptionsError>
    where
        T: serde::de::DeserializeOwned,
    {
        ensure_protocol_turn_options_schema_version(self.schema_version)?;
        serde_json::from_value(self.payload.clone()).map_err(ProtocolTurnOptionsError::Decode)
    }
}

pub(crate) mod facade_ops {
    use super::{PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION, ProtocolTurnOptions};

    /// Facade-internal operations for [`ProtocolTurnOptions`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ProtocolTurnOptionsFacadeOps {
        fn merged_with_override(&self, override_options: &Self) -> Self;
    }

    impl ProtocolTurnOptionsFacadeOps for ProtocolTurnOptions {
        fn merged_with_override(&self, override_options: &Self) -> Self {
            match (&self.payload, &override_options.payload) {
                (serde_json::Value::Object(base), serde_json::Value::Object(overrides)) => {
                    let mut payload = base.clone();
                    payload.extend(overrides.clone());
                    Self {
                        schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
                        payload: serde_json::Value::Object(payload),
                    }
                }
                _ => override_options.clone(),
            }
        }
    }
}

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
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{OtelTraceOptions, OtelTraceSink};
pub use lash_trace::{
    TraceAttachment, TraceContentBlock, TraceContext, TraceEffectEnvelopeDiffEntry,
    TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue, TraceError, TraceEvent,
    TraceLlmMessage, TraceLlmRequest, TraceLlmResponse, TracePromptComponent,
    TraceProviderRequestEvent, TraceProviderStreamEvent, TraceRuntimeStreamEvent, TraceTokenUsage,
    TraceToolSpec,
};
pub use llm::transport::ProviderFailureKind;
pub use model::{ModelLimits, ModelLimitsError, ModelSpec, ModelSpecBuilder};
pub use plugin::{
    AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord,
    AppendSessionNodesRequest, AppendSessionNodesResult, PluginError, PluginExtensions,
    PluginOptions, PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry,
    PluginSnapshotMeta, ProcessEngineContributionContext, ProtocolBeforeLlmCallContext,
    ProtocolLlmCallAction, SessionContextOverlay, SessionCreateRequest, SessionGraphService,
    SessionPluginSource, SessionReadView, SessionRelation, SessionSnapshot, SessionStartPoint,
    SessionStateService, SessionToolAccess, SubagentSessionContext,
};
pub(crate) use plugin::{
    OpenAgentFrameRequest, OpenAgentFrameResult, PluginRuntimeDirective, SessionTurnInput,
};

pub use provider::{
    CacheControlDialect, ModelCapability, ReasoningCapability, ReasoningDisableEncoding,
    ReasoningEncoding, ReasoningSelection, SamplingCapability, StreamTermination,
};
pub(crate) use provider::{
    EmptyProviderResolver, ProviderBinding, ProviderCompletion, ProviderCompletionError,
    ProviderResolutionError, RuntimeProviderResolver,
};
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestLocalProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestProcessRegistryWriteExt;
#[doc(hidden)]
pub use runtime::drive_with_event_pump;

pub use runtime::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AwaitEventKey, AwaitEventResolver,
    AwaitEventWaitIdentity, BoundaryReason, CausalRef, Clock, DeliveryPolicy, EffectHost,
    EffectJournalRetirement, ExecutionScope, ForkPoint, ForkSessionRequest, ForkSessionResult,
    InputItem, LiveReplayGapReason, LiveReplayResult, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeResult, LiveReplaySubscription, ObserverInheritance, PendingTurnInput,
    PendingTurnInputCancelOutcome, PendingTurnInputCancelResult, PendingTurnInputCancelTarget,
    PendingTurnInputClaimDiagnostics, PendingTurnInputDraft, PendingTurnInputSuffixCancelOutcome,
    PersistedSegmentHandover, ProcessAwaitOutput, ProcessCancelSummary, ProcessChange,
    ProcessChangeCursor, ProcessCompletionAuthority, ProcessCompletionOutcome,
    ProcessContinuationStore, ProcessEngine, ProcessEngineRunContext,
    ProcessEngineValidationContext, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessEventType, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleSummary, ProcessId, ProcessIdentity, ProcessInfraError,
    ProcessInput, ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion,
    ProcessListFilter, ProcessListMode, ProcessLiveReferenceSummary, ProcessObserverBy,
    ProcessOpScope, ProcessOriginator, ProcessOutcome, ProcessProvenance, ProcessPruneReport,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessRunOutcome, ProcessService,
    ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartOptions, ProcessStartOutcome,
    ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalSpec,
    ProcessTombstone, ProcessValueSelector, ProcessWakeDelivery, ProcessWakeSpec,
    ProcessWorklistCursor, ProcessWorklistPage, ProjectionWatermark, PromptUsage,
    ProtocolSessionExtension, ProtocolSessionExtensionHandle, ProtocolTurnExtension,
    ProtocolTurnExtensionHandle, RecoveryDisposition, Resolution, ResolveOutcome, RuntimeError,
    RuntimeErrorCause, RuntimeErrorCode, ScopedEffectController, SegmentHandover, SegmentProgress,
    SessionCursor, SessionCursorError, SessionId, SessionObservationEvent,
    SessionObservationEventPayload, SessionProcessEventKind, SessionQueueEventKind,
    SessionRevision, SessionScope, SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy,
    TokenLedgerEntry, ToolCallLaunch, TurnActivity, TurnActivityId, TurnCancelOriginHint,
    TurnContext, TurnEvent, TurnInput, TurnInputApplication, TurnInputCheckpointBoundary,
    TurnInputClaim, TurnInputClaimData, TurnInputClaimMode, TurnInputCompletion,
    TurnInputCompletionData, TurnInputIngress, TurnInputState, WaitKind, WaitState, WakeDelivery,
    WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier,
};
#[allow(unused_imports)]
pub(crate) use runtime::{
    LlmAttachmentSpec, ProcessEventSemantics, QueuedCheckpointTurnInput, QueuedCheckpointWork,
    QueuedTurnWork, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkClaimData, QueuedWorkCompletion, QueuedWorkCompletionData,
    QueuedWorkItem, QueuedWorkPayload, RuntimeReplay, RuntimeSubject, load_process_execution_env,
    materialize_process_event_semantics, persist_process_execution_env,
    prepare_process_event_append, prepare_process_registration, prepare_process_start,
    process_event_invocation, process_registration_fingerprint, process_wake_batch_draft,
    process_wake_batch_draft_with_policy, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text, require_event_replay,
};
pub(crate) use runtime::{
    ProcessEngineRunGuard, ProcessEngineRuntimeContext, QueuedWorkEnqueueOutcome,
};
#[cfg(any(test, feature = "testing"))]
pub(crate) use runtime::{apply_process_event_projection, fold_process_record};
pub(crate) use session_model::plugin_runtime_protocol_event;
pub use store::{TurnId, WorkClaim, WorkCompletion};
// Effect / process-control types consumed by external effect hosts (e.g.
// lash-restate's workflows) and their integration tests. Kept on the public
// surface; the rest of the runtime block above stays crate-internal.

pub use runtime::{
    CheckpointClaimSet, LlmRequestSpec, ProcessCommand, ProcessEffectOutcome,
    ProcessEventSemanticsSpec, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeEffectReplayMismatchSummary,
    RuntimeInvocation, RuntimeScope, RuntimeSessionState, ToolAttemptLaunch,
};
pub(crate) use runtime::{ToolAttemptEffectOutcome, ToolBatchEffectOutcome};

pub(crate) use session::RuntimeExecutionTracing;
pub(crate) use session::Session;
pub use session::{ExecRequest, RuntimeExecutionContext, SessionError};
pub(crate) use session_graph::SessionMessageTreeNode;
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SessionGraph, SessionNodePayload, SessionNodeRecord,
};
pub(crate) use session_model::RuntimeSessionPolicy;

pub use session_model::{ProtocolEvent, SessionHistoryRecord};
pub use session_model::{SessionPolicy, TurnBudget};
pub use store::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, BlobRef, GcReport,
    LeaseClaimNonce, LeaseOwnerIdentity, QueuedWorkStore, RuntimePersistence, SessionAdmission,
    SessionBinding, SessionCommitStore, SessionExecutionLease, SessionExecutionLeaseAcquisition,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseDisplacement, SessionExecutionLeaseRenewalInstallMismatch,
    SessionExecutionLeaseStore, SessionMeta, StoreError, StoreMaintenance, TurnInputStore,
    VacuumReport,
};
pub use store::{
    CommitBudget, CommitBudgetLimit, HydratedSessionCheckpoint, OperationId, RuntimeCommit,
    RuntimeTurnCommitStamp, RuntimeUsageDelta, RuntimeUsageDeltaIdentity,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphAppend, PersistedSessionRead, RuntimeCommitResult, SessionCheckpoint, SessionHeadMeta,
    SessionHeadPayload, ensure_supported_schema_version, load_persisted_session_state,
};
pub use tool_provider::{
    PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall, ToolCall, ToolContext,
    ToolExecutionGrant, ToolPrepareCall, ToolPrepareContext, ToolProvider,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_agent_frame_seed_is_rejected_at_the_serde_boundary() {
        let err = serde_json::from_value::<ToolControl>(serde_json::json!({
            "type": "switch_agent_frame",
            "frame_id": "delegate",
            "initial_nodes": [{ "not": "a session append node" }],
            "task": "continue the work"
        }))
        .expect_err("invalid seed cannot construct a tool control");

        assert!(err.to_string().contains("kind"), "unexpected error: {err}");
    }

    #[test]
    fn protocol_turn_options_missing_payload_deserializes_to_empty_object() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION
        }))
        .expect("deserialize options");

        assert!(options.is_empty());
        assert_eq!(options.schema_version, PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION);
        assert_eq!(options.payload, serde_json::json!({}));
    }

    #[test]
    fn protocol_turn_options_explicit_null_is_not_empty() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            "payload": null
        }))
        .expect("deserialize options");

        assert!(!options.is_empty());
        assert_eq!(options.payload, serde_json::Value::Null);
    }

    #[test]
    fn protocol_turn_options_missing_schema_version_rejects_preversioned_state() {
        let err =
            serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({ "payload": {} }))
                .expect_err("pre-versioned options should fail");

        assert!(
            err.to_string().contains(
                "missing schema_version and were written by unsupported pre-versioned state"
            ),
            "{err}"
        );
    }

    #[test]
    fn protocol_turn_options_unsupported_schema_version_rejects_state() {
        let err = serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION + 1,
            "payload": {}
        }))
        .expect_err("unsupported options version should fail");

        assert!(
            err.to_string().contains("is not supported by this binary"),
            "{err}"
        );
    }

    #[test]
    // Architecture lint: lexical public-surface guard, not behavior proof.
    fn lint_root_exports_do_not_reintroduce_removed_session_state_shapes() {
        let source = include_str!("lib.rs");
        let removed_envelope = ["SessionState", "Envelope"].concat();
        let removed_persisted = ["PersistedSession", "Snapshot"].concat();
        let removed_history_rewriter = ["History", "Rewriter"].concat();
        let removed_rewrite_trigger = ["Rewrite", "Trigger"].concat();
        let removed_rewrite_context = ["Rewrite", "Context"].concat();
        let removed_history_state = ["History", "State"].concat();
        let removed_history_metadata = ["History", "Rewrite", "Metadata"].concat();

        assert!(!source.contains(&removed_envelope));
        assert!(!source.contains(&removed_persisted));
        assert!(!source.contains(&removed_history_rewriter));
        assert!(!source.contains(&removed_rewrite_trigger));
        assert!(!source.contains(&removed_rewrite_context));
        assert!(!source.contains(&removed_history_state));
        assert!(!source.contains(&removed_history_metadata));
    }

    fn public_reexport_block(source: &str, module: &str) -> String {
        let start = format!("pub use {module}::{{");
        let mut block = String::new();
        let mut collecting = false;
        for line in source.lines() {
            if line.trim_start().starts_with(&start) {
                collecting = true;
            }
            if collecting {
                block.push_str(line);
                block.push('\n');
                if line.trim_end() == "};" {
                    break;
                }
            }
        }
        assert!(!block.is_empty(), "missing public {module} re-export block");
        block
    }

    #[test]
    // Architecture lint: lexical public-surface guard, not behavior proof.
    fn lint_root_runtime_exports_exclude_internal_runtime_records() {
        let runtime_exports = public_reexport_block(include_str!("lib.rs"), "runtime");
        for removed in [
            "RuntimeEffectCommand",
            "RuntimeEffectEnvelope",
            "RuntimeEffectKind",
            "RuntimeEffectOutcome",
            "RuntimeInvocation",
            "RuntimeScope",
            "RuntimeSessionState",
            "QueuedWorkBatch",
            "QueuedWorkBatchDraft",
            "QueuedWorkPayload",
            "prepare_process_registration",
            "process_wake_batch_draft",
            "require_event_replay",
        ] {
            assert!(
                !runtime_exports.contains(removed),
                "runtime root export leaked {removed}"
            );
        }
    }

    #[test]
    // Architecture lint: lexical public-surface guard, not behavior proof.
    fn lint_root_store_exports_exclude_wire_records() {
        let store_exports = public_reexport_block(include_str!("lib.rs"), "store");
        for removed in [
            "SessionHead",
            "SessionCheckpoint",
            "RuntimeCommit",
            "HydratedSessionCheckpoint",
            "PersistedSessionRead",
            "GraphAppend",
        ] {
            assert!(
                !store_exports.contains(removed),
                "store root export leaked {removed}"
            );
        }
    }

    #[test]
    // Architecture lint: lexical retired-name guard, not behavior proof.
    fn lint_removed_manager_and_host_trait_names_stay_removed() {
        let removed_manager = ["Runtime", "Session", "Manager"].concat();
        let removed_host = ["Runtime", "Session", "Host"].concat();
        let sources = [
            include_str!("runtime/session_manager/mod.rs"),
            include_str!("plugin/runtime_host.rs"),
            include_str!("tool_dispatch/context.rs"),
            include_str!("tool_provider.rs"),
        ];

        for source in sources {
            assert!(!source.contains(&removed_manager));
            assert!(!source.contains(&removed_host));
        }
    }
}
