pub mod attachment;
pub mod causal;
pub mod core_support;
mod effect_group;
mod effect_identity;
mod frame_key;
pub mod handle;
pub mod identity;
pub mod llm;
pub mod plugin;
pub mod process_cursor;
pub mod prompt;
mod redacted;
pub mod sansio;
pub mod schema_contract;
pub mod session;
pub mod session_model;
mod standard_batch;
pub mod sync;
pub mod tool_catalog;
pub mod tool_contract;
mod tool_intents;
pub mod tool_output;
pub mod turn;
pub mod turn_driver;
mod workflow;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Identity version mixed into every Lashlang and TypeScript module-artifact hash.
///
/// v16: the builtin registry gains `__typescript_pending_timer`, and
/// `__typescript_await_array` names its aggregate by method (ADR 0099 §11).
///
/// v17 (FIG-3571): module artifacts carry the shared carrier IR — structural
/// roles, process origins, binding visibility, source identity v4 and the
/// canonical number rule — so every artifact hash moves.
///
/// v18 (FIG-3620): the builtin registry gains `__typescript_global_get`, the
/// live root-global read every `globalThis.name` read lowers to.
///
/// v19 (FIG-3627): the standard-library dispatch gains `Lash.Apply`, which a
/// call with a spread argument to a builtin lowers to, so a module that spreads
/// into a builtin means nothing to a runtime without it.
///
/// v20 (FIG-3655): a function expression's canonical identity carries its
/// inferred ECMA `name`, so two programs identical but for a naming context
/// hash differently — exactly what the observable `f.name` difference means.
///
/// v21 (FIG-3701): a member read of an advertised method (`x.includes`) means
/// the built-in function object where it meant `undefined`, so an unchanged
/// artifact that reads one no longer means what it did.
pub const LASHLANG_SEMANTIC_HASH_VERSION: &str = "lashlang-semantic-v21";

pub use attachment::{
    AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, InvalidAttachmentId,
    InvalidMediaType, MediaType,
};
pub use causal::CausalRef;
pub use effect_group::GroupWakePolicy;
pub use effect_identity::{
    EffectAddress, EffectIdentityError, EffectJournalIdentity, ExecutionScope,
};
pub use frame_key::{FrameKey, FrameKeyError};
pub use handle::{
    HANDLE_FIELD, HANDLE_KIND, HandleId, HandleTarget, is_handle_shape, parse_handle,
};
pub use identity::{
    BatchId, InputId, NodeId, ProcessId, SessionId, TurnId, session_owner_namespace,
};
pub use llm::capability::{
    ModelCapability, ModelEffortValidationCategory, ModelEffortValidationError,
    ReasoningCapability, ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection,
};
pub use llm::types::{LlmTerminalReason, ProviderFailureKind};
pub use plugin::{
    CheckpointKind, PluginMessage, PluginRuntimeEvent, PromptContribution, PromptContributionBody,
    PromptContributionGate,
};
pub use process_cursor::{
    PROCESS_CURSOR_UNROUTED_EPOCH, PROCESS_CURSOR_VERSION, ProcessCursor, ProcessCursorError,
    ProcessCursorReference,
};
pub use prompt::{
    PreparedPrompt, PromptBuildInput, PromptCache, PromptContext, PromptContributionSet,
    PromptFingerprint, build_prompt, build_prompt_cached, prompt_template_fingerprint,
    prompt_text_fingerprint, prompt_tool_names_fingerprint,
};
pub use redacted::Redacted;
pub use sansio::{
    ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
    ContextProjector, DriverAction, DriverContextView, Effect, EffectId, LlmCallError,
    PendingToolCall, ProjectorContext, ProtocolDriverHandle, Response,
    TURN_CHECKPOINT_SCHEMA_VERSION, TurnCause, TurnCheckpoint, TurnCheckpointRestoreError,
    TurnMachine, TurnMachineConfig, TurnProtocol, UnitTurnProtocol, WaitingExecState,
    WaitingLlmState, render_turn_causes_prompt,
};
pub use schema_contract::{
    OmissionNullPath, OmissionNullPathSegment, ProjectionMode, ProviderSchemaCapabilities,
    ResolvedSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SchemaPurpose, SchemaResolutionError, SchemaResolutionRequest,
    project_anthropic_bedrock_schema, project_for_dialect, resolve_schema,
};
pub use session::{
    CellFailure, CellFailureKind, DegradedBinding, ExecCodeFailure, ExecCodeFailureReason,
    ExecResponse, ExecutedCall, ExecutedCallOutcome, ExecutedCallRecord, Observation,
    OmittedToolCalls, TextProjectionMetadata,
};
pub use session_model::message::{MessageOrigin, TurnOutputSource};
pub use session_model::{
    AcceptedInjectedTurnInput, BaseRenderCache, ConversationRecord, ErrorEnvelope, FailureCode,
    HostNamespace, InvalidNamespace, MAIN_AGENT_INTRO, Message, MessageRole, MessageSequence,
    Namespace, NoProgressBudget, Part, PartAttachment, PartKind, PromptBuiltin, PromptLayer,
    PromptSlot, PromptSlotLayer, PromptTemplate, PromptTemplateEntry, PromptTemplateSection,
    ProtocolEvent, RenderedPrompt, ResolvedPromptLayer, SessionAppendNode, SessionHistoryRecord,
    SessionStreamEvent, TokenUsage, TokenUsageOverflow, TurnBudget, TurnCancelDisposition,
    TurnCancelMode, TurnCancellationEvidence, TurnFailureCode, TurnFailureKind, TurnFinish,
    TurnOutcome, TurnStop, default_prompt_template, messages_are_prompt_resume_safe,
    resolve_prompt_layers, shared_parts,
};
pub use standard_batch::BatchResultRow;
pub use tool_catalog::{
    ToolCatalog, ToolCatalogBuildError, ToolCatalogBuildInput, ToolCatalogContribution,
    ToolCatalogEntry, ToolContractResolver, build_tool_catalog,
};
#[cfg(feature = "schema-validation")]
pub use tool_contract::validate_tool_input;
pub use tool_contract::{
    CompactToolContract, LashSchema, ModelTool, TYPESCRIPT_TOOL_BINDING_KEY, ToolActivation,
    ToolArgumentProjectionPolicy, ToolBinding, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolDiscovery, ToolId, ToolManifest, ToolOutputContract,
    ToolRetryPolicy, schema_for,
};
pub use tool_output::{
    AttachmentMaterializationNotice, AttachmentMaterializationReason,
    AttachmentMaterializationSource, CancelOrigin, CancelRequest, ModelToolReturn,
    ModelToolReturnPart, ObservedProcessFailure, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
    ToolCallStatus, ToolCancellation, ToolControl, ToolFailure, ToolFailureClass,
    ToolFailureSource, ToolIntentExecutionOutcome, ToolIntentIdentity, ToolIntentKind,
    ToolIntentRefusalReason, ToolRetryStatus, ToolValue, format_tool_output_content,
    model_parts_from_tool_output, tool_result_text,
};
pub use turn::{PreparedTurnMachine, SansIoTurnInput, build_turn};
pub use turn_driver::{
    TurnDriverConfig, TurnDriverPreamble, append_assistant_text_part, normalized_response_parts,
    reasoning_part, visible_response_parts, visible_response_text_from_parts,
};
pub use workflow::WorkflowExecutionSite;
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
