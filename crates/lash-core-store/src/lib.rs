//! Durable session layer of the Lash runtime kernel.
//!
//! This crate owns the session graph, the durable session state the runtime
//! commits, the commit-identity and checkpoint vocabulary, the attachment
//! layer, and the `SessionStore` trait family every backend implements. It
//! sits directly above `lash-core-llm` and below everything in `lash-core`
//! that drives a turn, so `lash-core` re-exports every module and item below
//! at its original path.

#![expect(
    clippy::expect_used,
    reason = "FIG-2784 pass 1, a later PR in the chain"
)]

/// Re-exported so `impl_noop_attachment_manifest!` can paste an
/// `#[async_trait]` impl into crates that do not depend on `async-trait`
/// directly. Not part of the supported surface.
#[doc(hidden)]
pub use async_trait::async_trait;

pub mod attachments;
pub mod await_event_identity;
pub mod chronological;
pub mod effect_identity;
pub mod execution_state;
pub mod input_normalization;
pub mod message_projection;
pub mod plugin_state;
pub mod process_identity;
pub mod protocol_turn_options;
pub mod queued_drain_policy;
pub mod queued_work_vocabulary;
pub mod runtime_error;
#[cfg(test)]
mod runtime_error_tests;
pub mod session_catalog;
pub mod session_execution_lease;
pub mod session_graph;
pub(crate) mod session_graph_integrity;
pub mod session_identity;
pub mod session_policy;
mod session_policy_serde;
pub mod session_read_view;
pub mod session_state;
pub mod session_store_factory_types;
pub mod store;
pub mod tool_state;
pub mod turn_control_binding;
pub mod turn_control_vocabulary;
pub mod turn_failure_evidence;
pub mod turn_input_vocabulary;
pub mod usage;

pub mod store_backend_support;

// Crate-root vocabulary. These re-exports exist so the moved modules keep the
// `crate::Item` paths they carried inside `lash-core`; they are deliberately
// crate-internal, so this crate's public surface is the modules alone.
pub(crate) use lash_core_ids::clock::{Clock, ClockWallTime, SystemClock};
#[cfg(feature = "perf-witness")]
pub(crate) use lash_core_ids::perf_witness;
pub(crate) use lash_core_ids::{operational_metrics, stable_hash, stable_identity};
pub(crate) use lash_core_llm::llm;
pub(crate) use lash_core_llm::model::ModelSpec;
pub(crate) use lash_core_llm::provider;
pub(crate) use lash_core_llm::session_model::ChargeSafetyPolicy;
pub(crate) use lash_sansio::llm::types::ChargeSafetyDecision;
pub(crate) use lash_sansio::llm::types::{
    AttachmentSource, ChargeSafetyDenialReason, GenerationOptions, LlmOutputPart, ProtocolPosition,
    ProviderFileScope,
};
pub(crate) use lash_sansio::session_model::{
    ConversationRecord, ProtocolEvent, PruneState, TurnBudget,
};
pub(crate) use lash_sansio::session_model::{
    NoProgressBudget, message::BaseRenderCache, message::MessageSequence,
};
pub(crate) use lash_sansio::tool_contract::{ToolDefinition, ToolId, ToolManifest};
pub(crate) use lash_sansio::{AcceptedInjectedTurnInput, PromptUsage};
pub(crate) use lash_sansio::{
    AttachmentId, AttachmentMaterializationNotice, AttachmentRef, AttachmentTypeMetadata, BatchId,
    CausalRef, CheckpointKind, EffectAddress, ExecutionScope, FrameKey, InputId, Message,
    MessageOrigin, MessageRole, NodeId, Part, PartKind, PluginMessage, ProcessId,
    PromptContribution, PromptLayer, SessionAppendNode, SessionId, TokenUsage, TurnId,
    TurnOutputSource, render_turn_causes_prompt, shared_parts,
};

pub(crate) type SessionHistoryRecord =
    lash_sansio::session_model::SessionHistoryRecord<ProtocolEvent>;

// Items the moved modules name at the crate root because `lash-core`'s
// `lib.rs` re-exported them there.
pub(crate) use attachments::SessionAttachmentStore;
pub(crate) use queued_drain_policy::{QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest};
pub(crate) use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SessionGraph, SessionNodePayload, SessionNodeRecord,
};
pub(crate) use store::{
    AppendRequestIdentity, AttachmentManifestEntry, AttachmentOwner, AttachmentWriteToken, BlobRef,
    CheckpointComponentDescriptor, GraphAppend, HydratedCheckpointComponent, LeaseOwnerIdentity,
    OperationId, QueuedWorkClaimOutcome, RuntimePersistence, SelectedQueuedWorkClaimOutcome,
    SessionExecutionLease, SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseObservation, SessionMeta, StoreError,
    WorkClaim,
};
pub(crate) use turn_failure_evidence::{TurnFailureEvidence, TurnFailureSettlement};
pub(crate) use usage::{LedgerUsageDisposition, TokenLedgerEntry, UnreportedLedgerAttempt};

pub(crate) use execution_state::{
    ExecutionStateComponentSnapshot, ExecutionStateSnapshot, HydratedExecutionState, PluginOptions,
};
pub(crate) use lash_sansio::{
    TurnCancelDisposition, TurnCancelMode, TurnCancellationEvidence, TurnCause,
};
pub(crate) use plugin_state::PluginState;
pub(crate) use process_identity::{
    ObserverInheritance, ProcessExecutionEnvSpec, ProcessIncarnation, ProcessRef, ProcessStatus,
};
pub(crate) use protocol_turn_options::ProtocolTurnOptions;
pub(crate) use queued_work_vocabulary::{
    DeliveryPolicy, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkClaimBoundary, QueuedWorkClaimData, QueuedWorkClaimPolicy, QueuedWorkCompletion,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, SessionCommand,
};
pub(crate) use runtime_error::{RuntimeError, RuntimeErrorCode};
pub(crate) use session_identity::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, SessionLineage,
    SessionObserverIntent, SessionObserverIntentAttribution, SessionRelation, SessionSnapshot,
    SessionToolAccess, SubagentSessionContext,
};
pub(crate) use session_policy::SessionPolicy;
pub(crate) use session_read_view::SessionReadView;
pub(crate) use session_state::RuntimeSessionState;
pub(crate) use tool_state::ToolState;
pub(crate) use turn_control_binding::StoreTurnCancellationAuthority;
pub(crate) use turn_control_vocabulary::{
    TurnAddress, TurnCancelClosureAuthorization, TurnCancelClosureAuthorizationOutcome,
    TurnCancelClosureSettlement, TurnCancelInputOutcome, TurnCancelIntentSnapshot,
    TurnCancelRequest, TurnCancelRequestRecord,
};
pub(crate) use turn_input_vocabulary::{
    PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt,
    PendingTurnInputCancelTarget, PendingTurnInputDraft, PendingTurnInputRead,
    PendingTurnInputSuffixCancelOutcome, TurnInputApplication, TurnInputClaimData,
    TurnInputCompletion, TurnInputIngress, TurnInputState,
};

pub(crate) use await_event_identity::{AwaitEventKey, AwaitEventWaitIdentity};
pub(crate) use chronological::ChronologicalProjection;
pub(crate) use effect_identity::{RuntimeEffectKind, RuntimeInvocation};
pub(crate) use lash_core_llm::provider::ProviderHandle;
pub(crate) use lash_sansio::ToolIntentIdentity;
pub(crate) use message_projection::plugin_message_to_message;
pub(crate) use process_identity::{ProcessWakeDelivery, WakeDeliveryState};
pub(crate) use queued_work_vocabulary::QueuedWorkClaim;
pub(crate) use session_graph::SessionGraphScopeError;
pub(crate) use store::queued_work::QueuedWorkClass;
pub(crate) use turn_control_vocabulary::TurnCancelOriginHint;
pub(crate) use turn_input_vocabulary::TurnInputCheckpointBoundary;
pub(crate) use turn_input_vocabulary::{InputItem, TurnContext, TurnInput};

/// Path shim: the moved modules keep the `crate::facade_support::*` paths they
/// carried inside `lash-core`. Only the facade operations whose receivers live
/// in this crate are reachable here.
pub(crate) mod facade_support {
    pub(crate) use crate::session_graph::facade_ops::SessionGraphFacadeOps;

    #[allow(unused_imports)]
    pub(crate) use crate::session_identity::facade_ops::AgentFrameReasonFacadeOps;
    pub(crate) use crate::tool_state::facade_ops::ToolStateFacadeOps;
    pub(crate) use lash_sansio::visible_response_text_from_parts;
}

/// Path shim: the durable session vocabulary `lash-core` exposes as
/// `crate::session_model`.
pub(crate) mod session_model {
    pub(crate) use crate::{
        ConversationRecord, Message, ProtocolEvent, SessionHistoryRecord, SessionPolicy,
        TokenUsage, plugin_message_to_message,
    };
    pub(crate) use lash_sansio::session_model::message;
}

/// Path shim: the durable half of what `lash-core` exposes as `crate::runtime`.
pub(crate) mod runtime {
    pub(crate) use crate::session_execution_lease;
    pub(crate) use crate::session_state as state;
    pub(crate) use crate::{
        PromptUsage, QueuedWorkBatch, QueuedWorkClaim, QueuedWorkClaimData, TurnInputClaimData,
    };

    #[allow(unused_imports)]
    pub(crate) use crate::turn_input_vocabulary::ingress_message_id;

    pub(crate) mod turn_input_ingress {
        pub use crate::turn_input_vocabulary::derive_pending_turn_input_id;
    }
}

pub(crate) use attachments::AttachmentSourcePolicy;
pub(crate) use input_normalization::NormalizedItem;
#[doc(hidden)]
pub use lash_sansio as sansio;
pub(crate) use lash_sansio::llm::capability::ReasoningSelection;
pub(crate) use lash_sansio::llm::types::LlmCallRecord;
pub(crate) use lash_sansio::session_model::prompt::{PromptSlot, PromptTemplate};
pub(crate) use process_identity::process_wake_turn_cause;
pub(crate) use runtime_error::RuntimeEffectReplayMismatchReport;
pub(crate) use session_graph::SessionMessageTreeNode;
pub(crate) use session_identity::{
    OpenAgentFrameRequest, OpenAgentFrameResult, SessionStoreCreateRequest,
};
pub(crate) use session_policy::ApplyConfigPatch;
pub(crate) use store::OrphanedTurnInputScope;
pub(crate) use store::work_claim::WorkCompletion;

#[allow(unused_imports)]
pub(crate) use attachments::AttachmentGcFence;
#[allow(unused_imports)]
pub(crate) use lash_core_ids::test_watchdog;
#[allow(unused_imports)]
pub(crate) use lash_sansio::attachment::MediaType;
#[allow(unused_imports)]
pub(crate) use lash_sansio::llm::capability::{ModelCapability, ReasoningRetentionPolicy};
#[allow(unused_imports)]
pub(crate) use lash_sansio::llm::capability::{
    OpenAiReasoningContext, ReasoningRetentionCapability, ReasoningRetentionSelection,
};
#[allow(unused_imports)]
pub(crate) use lash_sansio::llm::types::LlmResponse;
#[allow(unused_imports)]
pub(crate) use lash_sansio::tool_contract::ToolContract;
#[allow(unused_imports)]
pub(crate) use lash_sansio::tool_output::AttachmentMaterializationReason;
#[allow(unused_imports)]
pub(crate) use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainSelection, default_queued_drain_policy,
};
#[allow(unused_imports)]
pub(crate) use queued_work_vocabulary::QueuedWorkCompletionData;
#[allow(unused_imports)]
pub(crate) use queued_work_vocabulary::TurnWorkPayload;
#[allow(unused_imports)]
pub(crate) use session_graph::{
    SESSION_NODE_BODY_SCHEMA_VERSION, SharedJsonValue, build_active_read_projection,
    build_active_read_replacement, frame_node_id,
};
#[allow(unused_imports)]
pub(crate) use session_graph_integrity::graph_node_indices;
#[allow(unused_imports)]
pub(crate) use store::attachment_manifest::{
    AttachmentCondemnation, AttachmentDeleteArming, AttachmentIntent, AttachmentManifest,
    AttachmentWriteFence, AttachmentWritePermit,
};
#[allow(unused_imports)]
pub(crate) use store::commit_budget::{CommitBudget, CommitBudgetLimit};
#[allow(unused_imports)]
pub(crate) use store::runtime_commit::{RuntimeCommit, RuntimeTurnCommitStamp};
#[allow(unused_imports)]
pub(crate) use store::{SessionAdmission, SessionBinding};
#[allow(unused_imports)]
pub(crate) use turn_failure_evidence::ChargeSafetyRefusalEvidence;
#[allow(unused_imports)]
pub(crate) use turn_input_vocabulary::{TurnInputClaimMode, ingress_message_id};
#[allow(unused_imports)]
pub(crate) use turn_input_vocabulary::{TurnInputCompletionData, TurnInputSettlementClaim};

pub(crate) use turn_input_vocabulary::TurnActivityId;

/// Path shim: the durable half of what `lash-core` exposes as `crate::plugin`.
pub(crate) mod plugin {
    pub(crate) use crate::{
        ExecutionStateComponentSnapshot, ExecutionStateSnapshot, HydratedExecutionState,
    };
}
pub(crate) use attachments::AttachmentProducer;
pub(crate) use lash_sansio::attachment::AttachmentCreateMeta;

pub(crate) use runtime_error::RuntimeErrorCause;
pub(crate) use session_policy::GenerationOverlay;
