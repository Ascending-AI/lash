pub mod attachment;
pub mod causal;
#[doc(hidden)]
pub mod core_support;
mod frame_key;
pub mod llm;
pub mod plugin;
pub mod prompt;
pub mod sansio;
pub mod schema_contract;
pub mod session;
pub mod session_model;
mod standard_batch;
pub mod sync;
pub mod tool_catalog;
pub mod tool_contract;
pub mod tool_output;
pub mod turn;
pub mod turn_driver;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Identity version mixed into every Lashlang and TypeScript module-artifact hash.
pub const LASHLANG_SEMANTIC_HASH_VERSION: &str = "lashlang-semantic-v3";

pub use attachment::{
    AttachmentCreateMeta, AttachmentId, AttachmentMeta, AttachmentRef, AttachmentTypeMetadata,
    InvalidAttachmentId, InvalidMediaType, MediaType,
};
pub use causal::CausalRef;
pub use frame_key::{FrameKey, FrameKeyError};
pub use llm::capability::{
    ModelCapability, ModelEffortValidationCategory, ModelEffortValidationError,
    ReasoningCapability, ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection,
};
pub use llm::types::{LlmTerminalReason, ProviderFailureKind};
pub use plugin::{
    CheckpointKind, PluginMessage, PluginRuntimeEvent, PromptContribution, PromptContributionGate,
};
pub use prompt::{
    PreparedPrompt, PromptBuildInput, PromptCache, PromptContext, PromptContributionSet,
    PromptFingerprint, build_prompt, build_prompt_cached, prompt_template_fingerprint,
    prompt_text_fingerprint, prompt_tool_names_fingerprint,
};
pub use sansio::{
    ChatContextProjector, CheckpointDelivery, CheckpointResumeAction, CompletedToolCall,
    ContextProjector, DriverAction, DriverContextView, Effect, EffectId, LlmCallError,
    PendingToolCall, ProjectorContext, ProtocolDriverHandle, Response, TurnCause, TurnCheckpoint,
    TurnMachine, TurnMachineConfig, TurnProtocol, UnitTurnProtocol, WaitingExecState,
    WaitingLlmState, render_turn_causes_prompt,
};
pub use schema_contract::{
    ProjectionMode, ProviderSchemaCapabilities, ResolvedSchema, SchemaContract, SchemaDialect,
    SchemaProjectionOverride, SchemaProjectionPolicy, SchemaPurpose, SchemaResolutionError,
    SchemaResolutionRequest, project_anthropic_bedrock_schema, project_for_dialect, resolve_schema,
};
pub use session::{
    DegradedBinding, ExecResponse, ExecutedCallOutcome, ExecutedCallRecord, Observation,
    PromptUsage, TextProjectionMetadata,
};
pub use session_model::message::MessageOrigin;
pub use session_model::{
    AcceptedInjectedTurnInput, BaseRenderCache, ConversationRecord, ErrorEnvelope,
    MAIN_AGENT_INTRO, Message, MessageRole, MessageSequence, NoProgressBudget, Part,
    PartAttachment, PartKind, PromptBuiltin, PromptLayer, PromptSlot, PromptSlotLayer,
    PromptTemplate, PromptTemplateEntry, PromptTemplateSection, ProtocolEvent, PruneState,
    RenderedPrompt, ResolvedPromptLayer, SessionAppendNode, SessionHistoryRecord,
    SessionStreamEvent, TokenUsage, TokenUsageOverflow, TurnBudget, TurnCancelDisposition,
    TurnCancellationEvidence, TurnFinish, TurnOutcome, TurnStop, default_prompt_template,
    messages_are_prompt_resume_safe, resolve_prompt_layers, shared_parts,
};
#[doc(hidden)]
pub use standard_batch::BatchResultRow;
pub use tool_catalog::{
    ToolCatalog, ToolCatalogBuildInput, ToolCatalogContribution, ToolCatalogEntry,
    ToolContractResolver, build_tool_catalog,
};
pub use tool_contract::{
    CompactToolContract, LashSchema, ModelTool, ToolActivation, ToolArgumentProjectionPolicy,
    ToolContract, ToolDefinition, ToolId, ToolManifest, ToolOutputContract, ToolRetryPolicy,
    schema_for, validate_tool_input,
};
pub use tool_output::{
    ModelToolReturn, ModelToolReturnPart, ProcessParentEndPolicy, ToolCallOutcome, ToolCallOutput,
    ToolCallRecord, ToolCallStatus, ToolCancellation, ToolControl, ToolFailure, ToolFailureClass,
    ToolFailureSource, ToolIntentExecutionOutcome, ToolIntentIdentity, ToolIntentKind,
    ToolIntentParentEnd, ToolIntentParentEndAction, ToolIntentParentEndOutcome,
    ToolIntentRefusalReason, ToolRetryStatus, ToolValue, format_tool_output_content,
    model_parts_from_tool_output,
};
pub use turn::{PreparedTurnMachine, SansIoTurnInput, build_turn};
pub use turn_driver::{
    TurnDriverConfig, TurnDriverPreamble, TurnLimitFinalMessage, append_assistant_text_part,
    normalized_response_parts, reasoning_part, visible_response_parts,
    visible_response_text_from_parts,
};

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
