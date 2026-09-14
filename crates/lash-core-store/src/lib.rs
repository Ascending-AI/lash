//! Durable session layer of the Lash runtime kernel.
//!
//! This crate owns the session graph, the durable session state the runtime
//! commits, the commit-identity and checkpoint vocabulary, the attachment
//! layer, and the `SessionStore` trait family every backend implements. It
//! sits directly above `lash-core-llm` and below everything in `lash-core`
//! that drives a turn, so `lash-core` re-exports every module and item below
//! at its original path.

pub mod attachments;
pub mod execution_state;
pub mod plugin_state;
pub mod process_identity;
pub mod protocol_turn_options;
pub mod queued_drain_policy;
pub mod queued_work_vocabulary;
pub mod runtime_error;
pub mod session_identity;
pub mod session_policy;
pub mod session_read_view;
pub mod session_state;
pub mod session_graph;
pub(crate) mod session_graph_integrity;
pub mod session_graph_legacy_response;
pub mod store;
pub mod tool_state;
pub mod turn_control_binding;
pub mod turn_control_vocabulary;
pub mod turn_failure_evidence;
pub mod turn_input_vocabulary;
pub mod usage;

#[cfg(test)]
mod session_graph_tests;

pub mod store_backend_support;

// Crate-root vocabulary. These re-exports exist so the moved modules keep the
// `crate::Item` paths they carried inside `lash-core`; they are deliberately
// crate-internal, so this crate's public surface is the modules alone.
pub(crate) use lash_core_ids::clock::{Clock, ClockWallTime, SystemClock};
pub(crate) use lash_core_ids::{
    operational_metrics, stable_hash, stable_identity, task, test_watchdog,
};
#[cfg(feature = "perf-witness")]
pub(crate) use lash_core_ids::perf_witness;
pub(crate) use lash_core_llm::llm;
pub(crate) use lash_core_llm::model::ModelSpec;
pub(crate) use lash_core_llm::provider;
pub(crate) use lash_sansio::llm::types::{
    AttachmentSource, ChargeSafetyDenialReason, GenerationOptions, LlmOutputPart, LlmResponse,
    ProtocolPosition, ProviderFileScope,
};
pub(crate) use lash_sansio::session_model::{
    ConversationRecord, ProtocolEvent, PruneState, TurnBudget,
};
pub(crate) use lash_sansio::{
    AttachmentId, AttachmentMaterializationNotice, AttachmentMaterializationReason, AttachmentRef,
    AttachmentTypeMetadata, BatchId, CausalRef, CheckpointKind, EffectAddress, ExecutionScope,
    FrameKey, InputId, Message, MessageOrigin, MessageRole, NodeId, Part, PartKind, PluginMessage,
    ProcessId, PromptContribution, PromptLayer, SessionAppendNode, SessionId, TokenUsage, TurnId,
    TurnOutputSource, render_turn_causes_prompt, shared_parts,
};

pub(crate) type SessionHistoryRecord = lash_sansio::session_model::SessionHistoryRecord<ProtocolEvent>;

// Items the moved modules name at the crate root because `lash-core`'s
// `lib.rs` re-exported them there.
pub(crate) use attachments::{AttachmentGcFence, SessionAttachmentStore};
pub(crate) use queued_drain_policy::{
    QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest, QueuedDrainSelection,
    default_queued_drain_policy,
};
pub(crate) use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SessionGraph, SessionNodePayload, SessionNodeRecord,
    frame_node_id,
};
pub(crate) use store::{
    AttachmentCondemnation, AttachmentDeleteArming, AttachmentIntent, AttachmentManifest,
    AttachmentManifestEntry, AttachmentOwner, AttachmentWriteFence, AttachmentWritePermit,
    AttachmentWriteToken, BlobRef, CheckpointComponentDescriptor, CommitBudget, CommitBudgetLimit,
    GraphAppend, HydratedCheckpointComponent, LeaseOwnerIdentity, OperationId,
    QueuedWorkClaimOutcome, RuntimePersistence, RuntimeTurnCommitStamp, SelectedQueuedWorkClaimOutcome,
    SessionAdmission, SessionBinding, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseObservation, StoreError, WorkClaim,
};
pub(crate) use turn_failure_evidence::{TurnFailureEvidence, TurnFailureSettlement};
pub(crate) use usage::{LedgerUsageDisposition, TokenLedgerEntry, UnreportedLedgerAttempt};

pub(crate) use execution_state::{
    ExecutionStateComponentSnapshot, ExecutionStateSnapshot, HydratedExecutionState, PluginOptions,
};
pub(crate) use plugin_state::PluginState;
pub(crate) use process_identity::{
    ObserverInheritance, ProcessExecutionEnvSpec, ProcessIncarnation, ProcessRef, ProcessStatus,
};
pub(crate) use protocol_turn_options::ProtocolTurnOptions;
pub(crate) use queued_work_vocabulary::{
    DeliveryPolicy, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkClaimBoundary, QueuedWorkClaimData, QueuedWorkClaimPolicy, QueuedWorkCompletion,
    QueuedWorkCompletionData, QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind,
    QueuedWorkPayload, SessionCommand,
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
pub(crate) use turn_control_vocabulary::{
    TurnAddress, TurnCancelClosureAuthorization, TurnCancelClosureAuthorizationOutcome,
    TurnCancelClosureSettlement, TurnCancelInputOutcome, TurnCancelIntentSnapshot,
    TurnCancelRequest, TurnCancelRequestRecord, TurnCancellationAuthority,
};
pub(crate) use turn_input_vocabulary::{
    PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt,
    PendingTurnInputCancelTarget, PendingTurnInputDraft, PendingTurnInputSuffixCancelOutcome,
    TurnInputApplication, TurnInputClaimData, TurnInputCompletion, TurnInputCompletionData,
    TurnInputIngress, TurnInputState,
};
pub(crate) use lash_sansio::{
    TurnCancelDisposition, TurnCancelMode, TurnCancellationEvidence, TurnCause, TurnInput,
};
