pub mod append_vec;
pub mod attachment;
mod blake3_domains;
pub mod causal;
mod compat;
pub mod core_support;
pub mod definition_id;
mod effect_identity;
mod frame_key;
pub mod future;
pub mod handle;
pub mod identity;
pub mod json_decode;
pub mod json_schema;
mod run_aggregate;
pub use json_schema::{InvalidSchemaKind, JsonSchema, SchemaAdmissionError, ValueMismatch};
mod execution_budgets;
pub mod llm;
pub mod llm_profile;
pub mod module_artifact_refusal;
pub mod plugin;
pub mod profile;
mod redacted;
mod retained_output;
pub mod sansio;
pub mod schema_contract;
pub mod session;
pub mod session_model;
mod standard_batch;
pub mod sync;
mod tool_call_id;
pub mod tool_catalog;
pub mod tool_contract;
mod tool_declaration;
mod tool_intents;
pub mod tool_output;
pub mod turn;
pub mod turn_driver;
pub mod worker_limit;
mod workflow_site;

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
    MAX_EXECUTION_BUDGET, MAX_PROVIDER_ATTEMPTS, ProviderAttemptLimits,
};
pub use frame_key::{FrameKey, FrameKeyError};
pub use handle::{
    HANDLE_FIELD, HANDLE_KIND, HandleId, HandleTarget, is_handle_shape, parse_handle,
};
pub use identity::{
    BatchId, BlankIdentity, InputId, InvalidProcessId, NodeId, PROCESS_ID_PREFIX, ProcessId, RunId,
    RuntimeOwner, SessionId, TurnId, session_owner_namespace,
};
pub use llm::capability::{
    LlmProfileCapability, LlmProfileEffortValidationCategory, LlmProfileEffortValidationError,
    ReasoningCapability, ReasoningEncoding, ReasoningIntent, ReasoningSelection,
};
pub use llm::types::{LlmTerminalReason, ProviderFailureKind};
pub use plugin::{
    CheckpointKind, PluginFailureClass, PluginFailureOrigin, PluginHookFailure, PluginMessage,
    PluginOperationFailure, PluginRuntimeEvent, ToolCheckConflict, ToolCheckPhase, ToolCheckReply,
    ToolCheckVerdictKind,
};
pub use redacted::Redacted;
pub use retained_output::{OutputRetentionPolicy, OutputValue, RetainedOutput};
pub use run_aggregate::RunAggregateWakePolicy;
pub use sansio::{
    ChatContextProjector, CheckpointContentRef, CheckpointDelivery, CheckpointResumeAction,
    CompletedToolCall, ContextProjector, DriverAction, DriverContextView, Effect, EffectId,
    ExpandedRow, ExpandedWrapper, HeldControl, LlmCallError, ModelToolCalls, PendingToolCall,
    PendingWork, ProjectorContext, ProtocolDriverHandle, Response, ResponseToolCalls, SavedTurn,
    TURN_CHECKPOINT_SCHEMA_VERSION, ToolExpansionPlan, TurnCheckpoint, TurnCheckpointContent,
    TurnCheckpointRestoreError, TurnMachine, TurnMachineConfig, TurnProtocol, TurnWindow,
    TurnWindowPin, UndecodableDriverState, UnitTurnProtocol,
};
pub use schema_contract::{
    OmissionNullPath, OmissionNullPathSegment, ProjectionMode, ProviderSchemaCapabilities,
    ResolvedSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SchemaPurpose, SchemaResolutionError, SchemaResolutionRequest,
    project_anthropic_bedrock_schema, project_for_dialect, resolve_schema,
};
pub use session::{
    BindingChanges, CellControl, CellDefect, CellFailure, CellFailureKind, CellOutcome, CellPrint,
    CellRecord, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason, ExecResponse,
    ExecutedCall, ExecutedCallOutcome, OmittedToolCalls, TextProjectionMetadata,
};
pub use session_model::message::{MessageOrigin, TurnOutputSource, TurnReply, same_message};
pub use session_model::{
    AcceptedInjectedTurnInput, BaseRenderCache, CompletionCandidate, CompletionDisposition,
    ConversationRecord, ErrorEnvelope, FailureCode, HostNamespace, InternalPartKind,
    InvalidNamespace, LlmUsage, MaxToolCalls, Message, MessageRole, MessageSequence, Namespace,
    NoProgressBudget, Part, PartAttachment, PartKind, ProtocolEvent, RenderedPrompt,
    ReportedFailure, RetryProgress, SessionAppendNode, SessionHistoryRecord, SessionStreamEvent,
    StoredDataCorruption, StreamMessageKind, TerminationMode, TokenUsageOverflow,
    ToolCallLimitExceeded, ToolCallLimitScope, TurnBudget, TurnCancelMode,
    TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence, TurnFailureCode, TurnFailureKind,
    TurnFinish, TurnOutcome, TurnStop, messages_are_prompt_resume_safe, same_history_record,
    shared_parts,
};
pub use standard_batch::BatchResultRow;
pub use tool_call_id::{
    InvalidToolCallId, TOOL_CALL_ID_PREFIX, ToolCallAdmission, ToolCallId, ToolCallPosition,
    ToolCallRoot, ToolCallRootError,
};
pub use tool_catalog::{
    ToolCatalog, ToolCatalogBuildError, ToolCatalogBuildInput, ToolCatalogContribution,
    ToolCatalogEntry, ToolContractResolver, build_tool_catalog,
};
#[cfg(feature = "schema-validation")]
pub use tool_contract::validate_tool_input;
pub use tool_contract::{
    Backoff, BoundedRetry, CompactToolContract, ExecutionPolicy, ExtraKeys, LimitCause, ModelTool,
    ObjectShape, ParkBound, ProcessParamShape, ProcessShape, RegistrationRefused, SchemaShape,
    ShapeConstraints, ShapeField, ShapeKind, ShapeRow, TOOL_BINDING_KEY,
    ToolArgumentProjectionPolicy, ToolBinding, ToolBound, ToolBounds, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolDiscovery, ToolDraft, ToolId, ToolManifest, ToolModule,
    ToolOutputContract, ToolPresentationConfig, X_LASH_KEYWORD, XLashParam, XLashSignature,
    XLashType, is_named_type_reference, schema_for,
};
pub use tool_declaration::{
    DeclarationRefusal, OutcomeShape, ToolDeclaration, TurnControlKind, TurnControls,
};
pub use tool_output::{
    AttachmentMaterializationNotice, AttachmentMaterializationReason, CancelOrigin, CancelRequest,
    ModelToolReturn, ModelToolReturnPart, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
    ToolCallStatus, ToolCancellation, ToolControl, ToolFailure, ToolFailureCause, ToolFailureClass,
    ToolFailureSource, ToolIntentIdentity, ToolIntentKind, ToolValue, ToolView, ToolViewBlock,
    ToolViewMeta, TurnControl, format_tool_output_content, tool_result_text,
};
pub use turn::{PreparedTurnMachine, SansIoTurnInput, build_turn};
pub use turn_driver::{
    BuildNewestWriterFormats, TurnDriverConfig, TurnDriverPreamble, WriterFormats,
    append_assistant_text_part, build_newest_writer_formats, normalized_response_parts,
    reasoning_part, visible_response_parts, visible_response_text_from_parts,
};
pub use workflow_site::{
    EffectIdentity, LoopIteration, Site, SpawnIdentity, TaskIdentity, Unit, effect_identity_fixture,
};
mod execution_node_kind;
pub use execution_node_kind::ExecutionNodeKind;

pub fn head_tail_truncate(value: &str, max_chars: usize) -> (String, usize) {
    let raw_len = value.chars().count();
    if max_chars == 0 || raw_len <= max_chars {
        return (value.to_string(), raw_len);
    }
    let head_len = max_chars / 2;
    let tail_len = max_chars.saturating_sub(head_len);
    let head = value.chars().take(head_len).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let omitted = raw_len.saturating_sub(head_len + tail_len);
    (
        format!("{head}\n\n... ({omitted} characters omitted) ...\n\n{tail}"),
        raw_len,
    )
}
