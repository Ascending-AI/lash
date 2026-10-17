pub mod append_vec;
pub mod attachment;
mod blake3_domains;
pub mod causal;
mod codec_variants;
mod compat;
pub mod core_support;
pub mod definition_id;
mod derived;
mod effect_identity;
mod frame_key;
pub mod future;
pub mod handle;
pub mod identity;
pub mod json_decode;
pub mod json_schema;
mod run_aggregate;
pub use json_schema::{
    InvalidSchemaKind, JsonSchema, SchemaAdmissionError, ValueMismatch, tool_signature,
};
pub mod durable_serde;
mod execution_budgets;
pub mod llm;
pub mod llm_profile;
pub mod module_artifact_refusal;
pub mod plugin;
pub mod profile;
mod redacted;
mod result_fit;
mod retained_output;
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
pub use durable_serde::{Durable, DurableSerialize};
pub mod value_shape;
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
    CheckpointKind, ControlPhaseExpired, PluginFailureClass, PluginFailureOrigin,
    PluginHookFailure, PluginMessage, PluginOperationFailure, PluginRuntimeEvent,
    ToolCheckConflict, ToolCheckPhase, ToolCheckReply, ToolCheckVerdictKind,
};
pub use redacted::Redacted;
pub use retained_output::{OutputRetentionPolicy, OutputValue, RetainedOutput, RetainedValue};
pub use run_aggregate::RunAggregateWakePolicy;
pub use schema_contract::{
    OmissionNullPath, OmissionNullPathSegment, ProjectionMode, ProviderSchemaCapabilities,
    ResolvedSchema, SchemaContract, SchemaDialect, SchemaProjectionOverride,
    SchemaProjectionPolicy, SchemaPurpose, SchemaResolutionError, SchemaResolutionRequest,
    project_anthropic_bedrock_schema, project_for_dialect, resolve_schema,
};
pub use session::{
    BindingChanges, CallResultSummary, CellControl, CellDefect, CellFailure, CellFailureKind,
    CellFailureRepair, CellOutcome, CellPrint, CellRecord, DegradedBinding, ExecCodeFailure,
    ExecCodeFailureReason, ExecResponse, ExecutedCall, ExecutedCallOutcome, TextProjectionMetadata,
};
pub use session_model::message::{MessageOrigin, TurnOutputSource, TurnReply, same_message};
pub use session_model::{
    AcceptedInjectedTurnInput, BaseRenderCache, CompletionCandidate, CompletionDisposition,
    ConversationRecord, DecidedCompletion, ErrorEnvelope, FailureCode, HostNamespace,
    InternalPartKind, InvalidNamespace, LlmUsage, MaxToolCalls, Message, MessageRole,
    MessageSequence, Namespace, NoProgressBudget, Part, PartAttachment, PartKind, PluginStage,
    ProtocolEvent, ProtocolPayload, RenderedPrompt, ReportedFailure, RetryProgress,
    SessionAppendNode, SessionHistoryRecord, SessionStreamEvent, StoredDataCorruption,
    StreamMessageKind, TerminationMode, TokenUsageOverflow, ToolCallLimitExceeded,
    ToolCallLimitScope, TurnBudget, TurnCancelMode, TurnCancelUndeliveredInputPolicy,
    TurnCancellationEvidence, TurnErrorCause, TurnErrorCode, TurnFailureCode, TurnFailureKind,
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
    Backoff, BoundedRetry, ExecutionPolicy, LimitCause, ModelTool, ParkBound, RegistrationRefused,
    ToolArgumentProjectionPolicy, ToolBinding, ToolBound, ToolBounds, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolDiscovery, ToolDraft, ToolId, ToolManifest, ToolModule,
    ToolOutputContract, schema_for,
};
pub use tool_declaration::{
    DeclarationRefusal, FinishDeclaration, OutcomeShape, ToolDeclaration, TurnControlKind,
    TurnControls,
};
pub use tool_output::{
    AttachmentMaterializationNotice, AttachmentMaterializationReason, CancelOrigin, CancelRequest,
    ModelToolOutcome, ModelToolReturn, ModelToolReturnPart, NextRun, ToolCallOutcome,
    ToolCallOutput, ToolCallRecord, ToolCallStatus, ToolCancellation, ToolControl, ToolFailure,
    ToolFailureCause, ToolFailureClass, ToolFailureSource, ToolIntentIdentity, ToolIntentKind,
    ToolValue, ToolView, ToolViewBlock, ToolViewMeta, TurnControl,
};
pub use value_shape::{FieldShape, ValueShape, ValueShapeLimits};
#[cfg(any(test, feature = "testing"))]
pub use workflow_site::effect_identity_fixture;
pub use workflow_site::{EffectIdentity, LoopIteration, Site, SpawnIdentity, TaskIdentity, Unit};
mod execution_node_kind;
pub use execution_node_kind::ExecutionNodeKind;

pub mod trace_carrier;

pub use llm::types::{
    LlmCallError, append_assistant_text_part, normalized_response_parts, reasoning_part,
    visible_response_parts, visible_response_text_from_parts,
};

/// Serialized schema version for [`TurnCheckpoint`].
///
/// Version 1 is the historical unstamped checkpoint shape. Version 2 adds the
/// host-reporting effect for tool calls refused before dispatch. Version 3
/// removes the terminal-turn scheduling state; older checkpoints are not
/// compatible because replaying them could re-enter the deleted extra turn.
/// Version 4 was the typed-failure-code representation audit. Version 5
/// (FIG-3371) carries the streamed-block event vocabulary:
/// `SessionStreamEvent` inside `Effect::Emit` gained `stream_block_started` /
/// `stream_block_completed` and required `block` identities on text and
/// reasoning deltas, so v4 checkpoints holding pending text effects cannot
/// deserialize faithfully.
/// Version 6 carries parts-only plugin messages and removes part lifecycle fields.
/// Version 7 (FIG-3435) renames failure codes by
/// author: workspace-authored codes serialize as `lash:<spelling>` where
/// earlier checkpoints wrote `adapter:`/`refusal:`, and a v6 reader would
/// recolor a `lash:` code as provider vocabulary rather than refuse it.
/// Version 8 (FIG-3515) answers each tool call with one tool-result part
/// carrying ordered text and attachment `blocks`; v7 checkpoints holding
/// text-only results or call-bound attachment parts are refused.
/// Version 9 (FIG-3586) records the replay-key grammar the iteration's
/// execution-environment sync served (`synced_cell_replay_grammar`); a v8
/// checkpoint has no stamp, and a cell resumed from it would run under no
/// grammar.
/// Version 10 (FIG-3587) drops `update_machine_config` from the pending
/// execution-environment sync: every sync, the protocol-start one included,
/// now returns the environment the iteration's model call is built from, and
/// a v9 checkpoint parked on a host-only protocol-start sync would resume
/// with its first model call built from the live registry.
/// Version 11 (FIG-3571) drops `synced_cell_replay_grammar`: a turn's
/// executable generation is recorded once, at its admission, and checked
/// there before any effect, so no iteration carries a grammar of its own. A
/// v10 checkpoint's cells were fenced by a per-iteration stamp this build no
/// longer reads, so it is refused.
///
/// Version 11 changed in place before the 1.0 cut (FIG-5171, ruling #68): the
/// checkpoint holds bounded machine state and names its transcript (messages,
/// prompt view, history events and a pending model request) by content
/// digest; the content travels beside it as [`TurnCheckpointContent`].
/// It changed in place again (FIG-5206): it names the committed window the
/// turn started from by the host's pin ([`TurnWindow`]), and its content
/// holds only what the turn added to it.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-sansio/src/sansio/machine_state.rs",
///         path = "crates/lash-sansio/src/sansio/turn_protocol.rs",
///         cover(TurnCheckpoint, CheckpointState, CheckpointWork, Effect),
///     ),
///     roots(path = "crates/lash-protocol-types/src/session_model/message.rs", FlatPart, FlatPartRef),
/// )
/// version_surface = "drain"
/// format_manifest = "TurnCheckpoint"
pub const TURN_CHECKPOINT_SCHEMA_VERSION: u32 = 1;
