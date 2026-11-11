pub mod append_vec;
pub mod attachment;
mod blake3_domains;
pub mod causal;
mod codec_variants;
mod compat;
pub mod core_support;
pub mod definition_id;
mod derived;
pub mod durable_serde;
mod effect_identity;
mod execution_budgets;
mod frame_key;
pub mod future;
pub mod handle;
pub mod json_decode;
pub mod llm;
pub mod llm_profile;
pub mod module_artifact_refusal;
pub mod persistent_sequence;
pub mod plugin;
pub mod profile;
mod read_fact;
mod redacted;
pub use read_fact::{
    ReadFact, ReadFailure, ReadPanic, ReadResult, ToolCallClassification, ToolCallExecution,
    ToolReadCaller, ToolReadIdentity,
};
mod result_fit;
mod retained_output;
mod run_aggregate;
pub mod session;
pub mod session_model;
mod standard_batch;
pub mod sync;
mod tool_call_id;
pub mod tool_catalog;
mod tool_intents;
pub mod tool_output;
pub use durable_serde::{Durable, DurableSerialize};
pub mod value_shape;
pub mod worker_limit;
mod workflow_site;

/// The core transition after a protocol records a settled step.
#[derive(Clone, Debug)]
pub enum StepEnd {
    Continue,
    Completion(CompletionCandidate),
    Stop(TurnStop),
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The stock VM engine's stored kind, shared by definitions and registration.
pub const LASH_VM_ENGINE_KIND: &str = "lashvm";

pub use append_vec::AppendVec;
pub use attachment::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, InvalidAttachmentId,
    InvalidMediaType, MediaType,
};
pub use causal::CausalRef;
pub use compat::{VersionRange, VersionRangeError};
pub use definition_id::{
    DEFINITION_ID_FIELD, DEFINITION_ID_PREFIX, InvalidProcessDefinitionId, ProcessDefinitionId,
};
pub use effect_identity::{
    EffectAddress, EffectIdentityError, EffectJournalIdentity, ExecutionScope,
};
pub use execution_budgets::{
    ExecutionBudgets, ExecutionBudgetsConfig, ExecutionBudgetsError, ExecutionLimit,
    MAX_EXECUTION_BUDGET, MAX_PROVIDER_ATTEMPTS, ProviderAttemptLimits, RequestBound,
};
pub use frame_key::{FrameKey, FrameKeyError};
pub use handle::{
    HANDLE_FIELD, HANDLE_KIND, HandleId, HandleTarget, is_handle_shape, parse_handle,
};
/// Serde for an operation as its effect name, as a field and as map keys.
pub use llm::capability::{
    LlmProfileCapability, LlmProfileEffortValidationCategory, LlmProfileEffortValidationError,
    ReasoningCapability, ReasoningEncoding, ReasoningIntent, ReasoningSelection,
};
pub use llm::types::{LlmTerminalReason, ProviderFailureKind};
pub use plugin::{
    CheckpointKind, ControlPhaseExpired, PluginFailureClass, PluginFailureOrigin,
    PluginHookFailure, PluginMessage, PluginOperationFailure, PluginRuntimeEvent,
    ToolCheckConflict, ToolCheckPhase, ToolCheckReply, ToolCheckVerdictKind,
};
pub use redacted::Redacted;
pub use retained_output::{OutputRetentionPolicy, OutputValue, RetainedOutput, RetainedValue};
pub use run_aggregate::RunAggregateWakePolicy;
pub use session::{
    BindingChanges, CallResultSummary, CellConsumption, CellControl, CellDefect, CellFailure,
    CellFailureCause, CellFailureKind, CellFailureRepair, CellOutcome, CellPrint, CellRecord,
    CellResource, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason, ExecResponse,
    ExecutedCall, ExecutedCallOutcome, ExecutionBoundFact, TextProjectionMetadata,
};
pub use session_model::message::{MessageOrigin, TurnOutputSource, TurnReply, same_message};
pub use session_model::{
    AcceptedInjectedTurnInput, BaseRenderCache, CompletionCandidate, CompletionDisposition,
    ConversationRecord, DecidedCompletion, EffectRefusal, ErrorEnvelope, FailureCode,
    HostNamespace, InternalPartKind, InvalidNamespace, LlmUsage, MaxToolCalls, Message,
    MessageRole, MessageSequence, Namespace, NoProgressBudget, Part, PartAttachment, PartKind,
    PluginStage, ProtocolEvent, ProtocolPayload, RenderedPrompt, ReportedFailure, RetryProgress,
    SessionAppendNode, SessionHistoryRecord, SessionStreamEvent, StoredDataCorruption,
    StreamMessageKind, TerminationMode, TokenUsageOverflow, ToolCallLimitExceeded,
    ToolCallLimitScope, TurnBudget, TurnCancelMode, TurnCancelUndeliveredInputPolicy,
    TurnCancellationEvidence, TurnErrorCause, TurnErrorCode, TurnFailureCode, TurnFailureKind,
    TurnFinish, TurnOutcome, TurnStop, messages_are_prompt_resume_safe, same_history_record,
    shared_parts,
};
pub use standard_batch::{BatchResultRow, ModelBatchResultRow};
pub use tool_call_id::{
    InvalidToolCallId, TOOL_CALL_ID_PREFIX, ToolCallAdmission, ToolCallId, ToolCallPosition,
    ToolCallRoot, ToolCallRootError,
};
pub use tool_catalog::{
    ToolCatalog, ToolCatalogBuildInput, ToolCatalogContribution, ToolCatalogEntry,
    ToolContractResolver, build_tool_catalog,
};
pub use tool_output::{
    AttachmentMaterializationNotice, AttachmentMaterializationReason, CancelOrigin, CancelRequest,
    ModelToolOutcome, ModelToolReturn, ModelToolReturnPart, NextRun, NonFiniteToolViewPriority,
    ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolCallStatus, ToolCancellation, ToolControl,
    ToolFailure, ToolFailureCause, ToolFailureClass, ToolFailureSource, ToolIntentIdentity,
    ToolValue, ToolView, ToolViewBlock, ToolViewMeta, ToolViewPriority, TurnControl,
};
pub use value_shape::{FieldShape, ValueShape, ValueShapeLimits};
#[cfg(any(test, feature = "testing"))]
pub use workflow_site::effect_identity_fixture;

pub mod trace_carrier;

pub use llm::types::{
    LlmCallError, append_assistant_text_part, normalized_response_parts, reasoning_part,
    visible_response_parts, visible_response_text_from_parts,
};

/// The inputs and original build decisions that reconstruct an open turn.
///
/// version_guard(
///     roots(path = "crates/lash-sansio/src/sansio/input_journal.rs", MachineInput, JournalRow),
///     roots(path = "crates/lash-core/src/runtime/durable/turn_build.rs", TurnBuild),
/// )
/// version_surface = "drain"
/// format_manifest = "TurnJournal"
pub const TURN_JOURNAL_SCHEMA_VERSION: u32 = 2;

pub mod engine_fault;
