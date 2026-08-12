use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::CheckpointKind;
use crate::llm::types::{
    AttachmentSource, LlmEventSender, LlmMessage, LlmOutputSpec, LlmProviderTraceSender,
    LlmToolChoice, LlmToolSpec,
};
use crate::sansio::{CompletedToolCall, ExecutionEnvironmentSync, LlmCallError};
use crate::tool_dispatch::ToolTriggerEffectOutcome;
use crate::{
    AttachmentCreateMeta, CausalRef, CheckpointDelivery, ExecResponse,
    LlmRequest as CoreLlmRequest, LlmResponse, ProcessAwaitOutput, ProcessExecutionContext,
    ProcessListMode, ProcessRecord, ProcessRegistration, SessionScope,
};

use super::executor::RuntimeEffectControllerError;

const PROCESS_TRANSFER_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for process-transfer set identities.
///
/// Version 1 has no sum variants: it preserves the caller's ordered process
/// id sequence. Retired tags remain burned when variants are introduced.
fn process_transfer_set_preimage(process_ids: &[String]) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-transfer-set",
        PROCESS_TRANSFER_FAMILY_VERSION,
    );
    identity.sequence(process_ids, |identity, process_id| {
        identity.string(process_id);
    });
    identity.finish()
}

fn process_transfer_set_identity(process_ids: &[String]) -> String {
    crate::stable_identity::rendered_hash(
        "process-transfer-set",
        PROCESS_TRANSFER_FAMILY_VERSION,
        &process_transfer_set_preimage(process_ids),
    )
}

/// Durable category for a runtime effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEffectKind {
    LlmCall,
    Direct,
    ToolAttempt,
    ToolBatch,
    Trigger,
    Process,
    ExecCode,
    Checkpoint,
    SyncExecutionEnvironment,
    Sleep,
    AwaitEvent,
    PeekAwaitEvent,
}

impl RuntimeEffectKind {
    /// Exposes the stable snake-case kind label effect-host implementors persist in replay
    /// diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlmCall => "llm_call",
            Self::Direct => "direct",
            Self::ToolAttempt => "tool_attempt",
            Self::ToolBatch => "tool_batch",
            Self::Trigger => "trigger",
            Self::Process => "process",
            Self::ExecCode => "exec_code",
            Self::Checkpoint => "checkpoint",
            Self::SyncExecutionEnvironment => "sync_execution_environment",
            Self::Sleep => "sleep",
            Self::AwaitEvent => "await_event",
            Self::PeekAwaitEvent => "peek_await_event",
        }
    }
}

/// Canonical lineage for a runtime-side invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInvocation {
    pub scope: RuntimeScope,
    pub subject: RuntimeSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<RuntimeReplay>,
}

impl RuntimeInvocation {
    /// Constructs a replay-scoped effect invocation for effect-host implementors, binding the
    /// effect ID, kind, and replay key before nondeterministic work begins.
    pub fn effect(
        scope: RuntimeScope,
        effect_id: impl Into<String>,
        kind: RuntimeEffectKind,
        replay_key: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            subject: RuntimeSubject::Effect {
                effect_id: effect_id.into(),
                kind,
            },
            caused_by: None,
            replay: Some(RuntimeReplay {
                key: replay_key.into(),
            }),
        }
    }

    /// Sets the caused by carried by a `RuntimeInvocation` for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn with_caused_by(mut self, caused_by: Option<CausalRef>) -> Self {
        self.caused_by = caused_by;
        self
    }

    /// Exposes the effect ID to effect-host implementors only for effect subjects, returning `None`
    /// for process, trigger, and session-node subjects.
    pub fn effect_id(&self) -> Option<&str> {
        match &self.subject {
            RuntimeSubject::Effect { effect_id, .. } => Some(effect_id),
            _ => None,
        }
    }

    /// Exposes the declared kind to effect-host implementors only for effect subjects, returning
    /// `None` for other invocation subjects.
    pub fn effect_kind(&self) -> Option<RuntimeEffectKind> {
        match &self.subject {
            RuntimeSubject::Effect { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Exposes replay key to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn. Returns `None` when no replay key is present.
    pub fn replay_key(&self) -> Option<&str> {
        self.replay.as_ref().map(|replay| replay.key.as_str())
    }

    /// Projects stable causal identity for protocol and effect-host implementors; each invocation
    /// subject maps to its corresponding causal-reference variant.
    pub fn causal_ref(&self) -> Option<CausalRef> {
        match &self.subject {
            RuntimeSubject::Effect { effect_id, .. } => Some(CausalRef::Effect {
                session_id: self.scope.session_id.clone(),
                turn_id: self.scope.turn_id.clone(),
                effect_id: effect_id.clone(),
            }),
            RuntimeSubject::Process { process_id } => Some(CausalRef::Process {
                process_id: process_id.clone(),
            }),
            RuntimeSubject::ProcessEvent {
                process_id,
                sequence,
                ..
            } => Some(CausalRef::ProcessEvent {
                process_id: process_id.clone(),
                sequence: *sequence,
            }),
            RuntimeSubject::TriggerOccurrence { occurrence_id } => {
                Some(CausalRef::TriggerOccurrence {
                    occurrence_id: occurrence_id.clone(),
                    subscription_id: None,
                    subscription_incarnation: None,
                    subscription_revision: None,
                })
            }
            RuntimeSubject::SessionNode { node_id } => Some(CausalRef::SessionNode {
                session_id: self.scope.session_id.clone(),
                node_id: node_id.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeScope {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl RuntimeScope {
    /// Constructs a `RuntimeScope` for store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: None,
            turn_index: None,
            protocol_iteration: None,
        }
    }

    /// Constructs the complete turn scope effect-host implementors persist with an effect,
    /// including turn index and protocol iteration.
    pub fn for_turn(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        turn_index: usize,
        protocol_iteration: usize,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: Some(turn_id.into()),
            turn_index: Some(turn_index),
            protocol_iteration: Some(protocol_iteration),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReplay {
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeSubject {
    Effect {
        effect_id: String,
        kind: RuntimeEffectKind,
    },
    Process {
        process_id: String,
    },
    ProcessEvent {
        process_id: String,
        sequence: u64,
        event_type: String,
    },
    TriggerOccurrence {
        occurrence_id: String,
    },
    SessionNode {
        node_id: String,
    },
}

/// Fully serializable envelope emitted at Lash's nondeterministic boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeEffectEnvelope {
    pub invocation: RuntimeInvocation,
    pub command: RuntimeEffectCommand,
}

// Measured 448 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<RuntimeEffectEnvelope>() <= 576);

impl RuntimeEffectEnvelope {
    /// Constructs a validated effect envelope for effect-host implementors and panics if the
    /// invocation and command violate the durable-effect contract.
    pub fn new(invocation: RuntimeInvocation, command: RuntimeEffectCommand) -> Self {
        Self::try_new(invocation, command).expect("valid runtime effect invocation")
    }

    /// Validates and constructs an effect envelope for effect-host implementors: the subject must
    /// be a non-empty effect with a replay key and matching command kind, and tool attempts and
    /// batches must carry valid indices and IDs.
    pub fn try_new(
        invocation: RuntimeInvocation,
        command: RuntimeEffectCommand,
    ) -> Result<Self, RuntimeEffectControllerError> {
        validate_effect_invocation(&invocation, command.kind())?;
        validate_effect_command(&command)?;
        Ok(Self {
            invocation,
            command,
        })
    }

    /// Hashes the canonical envelope for effect-host implementors so replay comparison is stable
    /// across equivalent serialized representations.
    pub fn stable_hash(&self) -> Result<String, RuntimeEffectControllerError> {
        Ok(self.canonical_form()?.hash().to_string())
    }

    /// Captures the canonical replay-comparison form for effect-host implementors without depending
    /// on ordinary serde field ordering.
    pub fn canonical_form(
        &self,
    ) -> Result<super::CanonicalRuntimeEffectEnvelope, RuntimeEffectControllerError> {
        super::CanonicalRuntimeEffectEnvelope::capture(self)
    }
}

fn validate_effect_invocation(
    invocation: &RuntimeInvocation,
    command_kind: RuntimeEffectKind,
) -> Result<(), RuntimeEffectControllerError> {
    let RuntimeSubject::Effect { effect_id, kind } = &invocation.subject else {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
            "runtime effect envelope subject must be an effect",
        ));
    };
    if effect_id.trim().is_empty() {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
            "runtime effect envelope effect id must be non-empty",
        ));
    }
    if *kind != command_kind {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectInvocationKind,
            format!(
                "runtime effect invocation kind {} does not match command kind {}",
                kind.as_str(),
                command_kind.as_str()
            ),
        ));
    }
    if invocation
        .replay
        .as_ref()
        .is_none_or(|replay| replay.key.is_empty())
    {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectReplayRequired,
            "runtime effect envelope requires replay.key",
        ));
    }
    Ok(())
}

fn validate_effect_command(
    command: &RuntimeEffectCommand,
) -> Result<(), RuntimeEffectControllerError> {
    if let RuntimeEffectCommand::ToolAttempt {
        call,
        execution_grant: _,
        attempt,
        max_attempts,
    } = command
    {
        if call.call_id.trim().is_empty() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolAttemptCallId,
                "runtime effect tool attempt requires a non-empty call id",
            ));
        }
        if *attempt == 0 || *max_attempts == 0 || *attempt > *max_attempts {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolAttemptIndex,
                format!(
                    "runtime effect tool attempt must satisfy 1 <= attempt <= max_attempts, got {attempt}/{max_attempts}"
                ),
            ));
        }
    }
    if let RuntimeEffectCommand::ToolBatch { batch } = command {
        if batch.batch_id.trim().is_empty() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolBatchId,
                "runtime effect tool batch id must be non-empty",
            ));
        }
        if batch.calls.is_empty() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolBatchEmpty,
                "runtime effect tool batch must contain at least one prepared call",
            ));
        }
        for (index, call) in batch.calls.iter().enumerate() {
            if call.call.call_id.trim().is_empty() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolBatchCallId,
                    format!("runtime effect tool batch call {index} has an empty call id"),
                ));
            }
            if call.replay_suffix.trim().is_empty() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectToolBatchCallReplay,
                    format!("runtime effect tool batch call {index} has an empty replay suffix"),
                ));
            }
        }
    }
    Ok(())
}

/// Serializable command emitted at Lash's nondeterministic runtime boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffectCommand {
    LlmCall {
        request: Box<LlmRequestSpec>,
    },
    Direct {
        request: Box<LlmRequestSpec>,
        usage_source: String,
    },
    ToolAttempt {
        call: crate::PreparedToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
    },
    ToolBatch {
        batch: crate::PreparedToolBatch,
    },
    Trigger {
        command: Box<crate::TriggerCommand>,
    },
    Process {
        command: Box<ProcessCommand>,
    },
    ExecCode {
        language: String,
        code: String,
    },
    Checkpoint {
        checkpoint: CheckpointKind,
    },
    SyncExecutionEnvironment {
        update_machine_config: bool,
    },
    Sleep {
        duration_ms: u64,
    },
    AwaitEvent {
        key: crate::AwaitEventKey,
    },
    PeekAwaitEvent {
        key: crate::AwaitEventKey,
    },
}

// Measured 200 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<RuntimeEffectCommand>() <= 256);

impl RuntimeEffectCommand {
    /// Boxes one process command at the effect boundary for effect-host and process-engine
    /// implementors so the durable envelope remains size-bounded.
    pub fn process(command: ProcessCommand) -> Self {
        Self::Process {
            command: Box::new(command),
        }
    }

    /// Exposes kind to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,
            Self::ToolBatch { .. } => RuntimeEffectKind::ToolBatch,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::Sleep { .. } => RuntimeEffectKind::Sleep,
            Self::AwaitEvent { .. } => RuntimeEffectKind::AwaitEvent,
            Self::PeekAwaitEvent { .. } => RuntimeEffectKind::PeekAwaitEvent,
        }
    }
}

/// Serializable operation against the process admin plane.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// justification: RuntimeEffectCommand already boxes every ProcessCommand, so boxing its Start payload again is redundant.
#[allow(clippy::large_enum_variant)]
pub enum ProcessCommand {
    Start {
        registration: ProcessRegistration,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        observers: Vec<String>,
        #[serde(
            default,
            skip_serializing_if = "boxed_process_execution_context_is_empty"
        )]
        execution_context: Box<ProcessExecutionContext>,
    },
    List {
        session_scope: SessionScope,
        #[serde(default)]
        mode: ProcessListMode,
    },
    Transfer {
        from_scope: SessionScope,
        to_scope: SessionScope,
        process_ids: Vec<String>,
    },
    DeleteSession {
        session_id: String,
    },
    Await {
        process_id: String,
    },
    Cancel {
        process_id: String,
        reason: Option<String>,
    },
    Signal {
        process_id: String,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    },
    EmitEvent {
        process_id: String,
        request: crate::ProcessEventAppendRequest,
    },
}

fn boxed_process_execution_context_is_empty(context: &ProcessExecutionContext) -> bool {
    context.is_empty()
}

type CheckpointOutcome = Result<CheckpointDelivery, RuntimeEffectControllerError>;

#[doc(hidden)]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointClaimSet {
    // Checkpoint replay skips the local executor that acquired these claims.
    // Journal them with the delivery so the replaying turn carries the same
    // settlement authority into its atomic final commit. Outcomes written by
    // older binaries have no claim set: one queued-work row and one active
    // turn-input row per in-flight turn can be redelivered by the next lease
    // generation, without loss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) queued_work_claims: Vec<crate::QueuedWorkClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_input_claim: Option<crate::TurnInputClaim>,
}

impl ProcessCommand {
    /// Derives the stable effect ID process-engine and effect-host implementors use to journal this
    /// process command without conflating command kinds.
    pub fn effect_id(&self) -> String {
        match self {
            Self::Start { registration, .. } => format!("process:start:{}", registration.id),
            Self::List {
                session_scope,
                mode,
            } => {
                format!("process:list:{}:{}", session_scope.id(), mode.as_str())
            }
            Self::Transfer {
                from_scope,
                to_scope,
                process_ids,
            } => {
                let digest = process_transfer_set_identity(process_ids);
                format!(
                    "process:transfer:{}:{}:{digest}",
                    from_scope.id(),
                    to_scope.id()
                )
            }
            Self::DeleteSession { session_id } => format!("process:delete-session:{session_id}"),
            Self::Await { process_id } => format!("process:await:{process_id}"),
            Self::Cancel { process_id, .. } => format!("process:cancel:{process_id}"),
            Self::Signal {
                process_id,
                signal_name,
                signal_id,
                ..
            } => {
                format!("process:signal:{process_id}:signal.{signal_name}:{signal_id}")
            }
            Self::EmitEvent {
                process_id,
                request,
            } => format!(
                "process:emit-event:{process_id}:{}",
                request
                    .replay
                    .as_ref()
                    .map(|replay| replay.key.as_str())
                    .unwrap_or("missing-replay-key")
            ),
        }
    }
}

/// Serializable result of a process operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProcessEffectOutcome {
    Start {
        // Boxed so the fat durable record does not size the whole outcome enum
        // (and the runtime effect enum wrapping it) inline through the recursive
        // effect executor.
        record: Box<ProcessRecord>,
    },
    List {
        entries: Vec<ProcessRecord>,
    },
    Transfer,
    DeleteSession {
        report: crate::ProcessSessionDeleteReport,
    },
    Await {
        // Keep the full terminal record while bounding every process outcome
        // carried through the recursive effect executor.
        output: Box<ProcessAwaitOutput>,
    },
    Cancel {
        record: Box<ProcessRecord>,
    },
    Signal {
        // Boxed for the same reason as the record variants: a fat event should
        // not size the outcome enum inline through the recursive executor.
        event: Box<crate::ProcessEvent>,
    },
    EmitEvent {
        event: Box<crate::ProcessEvent>,
    },
}

// Measured 88 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<ProcessEffectOutcome>() <= 112);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolAttemptEffectOutcome {
    pub launch: ToolAttemptLaunch,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<ToolTriggerEffectOutcome>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolBatchEffectOutcome {
    pub launches: Vec<ToolCallLaunch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<ToolTriggerEffectOutcome>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallLaunch {
    Done {
        result: Box<CompletedToolCall>,
    },
    Pending {
        // Boxed for the same reason `Done` boxes its payload: the canonical
        // `ExecutionScope` inside the key dominates this enum's size.
        key: Box<crate::AwaitEventKey>,
        pending: crate::PendingCompletion,
        duration_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolAttemptLaunch {
    Done {
        record: Box<crate::ToolCallRecord>,
        intents: crate::ToolIntents,
    },
    Pending {
        // See `ToolCallLaunch::Pending`.
        key: Box<crate::AwaitEventKey>,
        pending: crate::PendingCompletion,
        duration_ms: u64,
    },
}

pub type RuntimeLlmCallOutcome = (
    Result<LlmResponse, LlmCallError>,
    bool,
    Option<crate::LlmCallRecord>,
);

pub type RuntimeDirectLlmOutcome = (
    Result<LlmResponse, LlmCallError>,
    Option<crate::LlmCallRecord>,
);

/// Serializable result of a runtime effect command.
///
/// Large payloads stay boxed so this boundary type remains cheap to retain in
/// nested async controller frames. `Box<T>` is serde-transparent, so durable
/// journal records keep their established JSON shape and full-record evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffectOutcome {
    LlmCall {
        result: Box<Result<LlmResponse, LlmCallError>>,
        text_streamed: bool,
        /// Sealed provider-attempt history. Older journal entries and calls
        /// interrupted before the provider handle returns have no record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_record: Option<crate::LlmCallRecord>,
    },
    Direct {
        result: Box<Result<LlmResponse, LlmCallError>>,
        /// Sealed provider-attempt history for this single direct call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_record: Option<crate::LlmCallRecord>,
    },
    ToolAttempt {
        launch: Box<ToolAttemptLaunch>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        triggers: Vec<ToolTriggerEffectOutcome>,
    },
    ToolBatch {
        launches: Vec<ToolCallLaunch>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        triggers: Vec<ToolTriggerEffectOutcome>,
    },
    Trigger {
        result: Box<crate::TriggerEffectResult>,
    },
    Process {
        result: ProcessEffectOutcome,
    },
    ExecCode {
        result: Box<Result<ExecResponse, String>>,
    },
    Checkpoint {
        result: CheckpointOutcome,
        #[serde(default)]
        claims: Box<CheckpointClaimSet>,
    },
    SyncExecutionEnvironment {
        result: Result<Option<ExecutionEnvironmentSync>, String>,
    },
    Sleep,
    AwaitEvent {
        resolution: crate::Resolution,
    },
    PeekAwaitEvent {
        resolution: Option<crate::Resolution>,
    },
}

// Measured 96 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<RuntimeEffectOutcome>() <= 128);

// =============================================================================
// Request specs (serializable forms of LLM/Direct requests)
// =============================================================================

/// Serializable attachment data for runtime effect envelopes.
///
/// Inline sources are normalized to `Stored` before this durable shape is
/// created. Borrowed sources round-trip without entering Lash storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmAttachmentSpec {
    pub source: AttachmentSource,
}

/// Serializable LLM request data. Live stream and provider-trace callbacks are
/// attached by the local executor, and attachment bytes are resolved locally
/// from refs rather than persisted in the effect envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmRequestSpec {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    pub attachments: Vec<LlmAttachmentSpec>,
    pub tools: Arc<Vec<LlmToolSpec>>,
    pub tool_choice: LlmToolChoice,
    pub model_variant: crate::ReasoningSelection,
    #[serde(default)]
    pub model_capability: crate::ModelCapability,
    #[serde(default)]
    pub generation: crate::GenerationOptions,
    pub scope: crate::LlmRequestScope,
    pub output_spec: Option<LlmOutputSpec>,
}

impl LlmRequestSpec {
    pub(crate) async fn from_request(
        request: &CoreLlmRequest,
        attachment_store: &crate::SessionAttachmentStore,
    ) -> Result<Self, RuntimeEffectControllerError> {
        Ok(Self {
            model: request.model.clone(),
            messages: request.messages.clone(),
            attachments: attachment_specs_from_attachments(&request.attachments, attachment_store)
                .await?,
            tools: Arc::clone(&request.tools),
            tool_choice: request.tool_choice.clone(),
            model_variant: request.model_variant.clone(),
            model_capability: request.model_capability.clone(),
            generation: request.generation.clone(),
            scope: request.scope.clone(),
            output_spec: request.output_spec.clone(),
        })
    }

    pub(crate) fn into_request(
        self,
        stream_events: Option<LlmEventSender>,
        provider_trace: Option<LlmProviderTraceSender>,
    ) -> CoreLlmRequest {
        CoreLlmRequest {
            model: self.model,
            messages: self.messages,
            attachments: self
                .attachments
                .into_iter()
                .map(|spec| spec.source)
                .collect(),
            resolved_stored: Default::default(),
            tools: self.tools,
            tool_choice: self.tool_choice,
            model_variant: self.model_variant,
            model_capability: self.model_capability,
            generation: self.generation,
            scope: self.scope,
            output_spec: self.output_spec,
            stream_events,
            provider_trace,
        }
    }
}

async fn attachment_specs_from_attachments(
    attachments: &[AttachmentSource],
    attachment_store: &crate::SessionAttachmentStore,
) -> Result<Vec<LlmAttachmentSpec>, RuntimeEffectControllerError> {
    let mut specs = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        specs.push(attachment_spec_from_attachment(attachment, attachment_store).await?);
    }
    Ok(specs)
}

async fn attachment_spec_from_attachment(
    attachment: &AttachmentSource,
    attachment_store: &crate::SessionAttachmentStore,
) -> Result<LlmAttachmentSpec, RuntimeEffectControllerError> {
    let source = match attachment {
        AttachmentSource::Inline { media_type, bytes } => {
            let attachment_ref = attachment_store
                .put(
                    bytes.clone(),
                    AttachmentCreateMeta::new(media_type.clone(), None, None),
                )
                .await
                .map_err(|err| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectAttachmentStore,
                        format!(
                            "failed to store attachment before runtime effect invocation: {err}"
                        ),
                    )
                })?;
            AttachmentSource::stored(attachment_ref)
        }
        durable => durable.clone(),
    };
    Ok(LlmAttachmentSpec { source })
}

impl RuntimeEffectOutcome {
    pub(crate) fn into_llm_call(
        self,
    ) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError> {
        match self {
            Self::LlmCall {
                result,
                text_streamed,
                call_record,
            } => Ok((*result, text_streamed, call_record)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LlmCall,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_direct_response(
        self,
    ) -> Result<RuntimeDirectLlmOutcome, RuntimeEffectControllerError> {
        match self {
            Self::Direct {
                result,
                call_record,
            } => Ok((*result, call_record)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Direct,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_tool_attempt_effect(
        self,
    ) -> Result<ToolAttemptEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::ToolAttempt { launch, triggers } => Ok(ToolAttemptEffectOutcome {
                launch: *launch,
                triggers,
            }),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolAttempt,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_tool_batch_effect(
        self,
    ) -> Result<ToolBatchEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::ToolBatch { launches, triggers } => {
                Ok(ToolBatchEffectOutcome { launches, triggers })
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolBatch,
                other.kind(),
            )),
        }
    }

    /// Extracts the process outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_process(self) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::Process { result } => Ok(result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Process,
                other.kind(),
            )),
        }
    }

    /// Extracts the trigger outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_trigger(self) -> Result<crate::TriggerEffectResult, RuntimeEffectControllerError> {
        match self {
            Self::Trigger { result } => Ok(*result),
            other => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!("expected trigger outcome, got {}", other.kind().as_str()),
            )),
        }
    }

    pub(crate) fn into_exec_code(
        self,
    ) -> Result<Result<ExecResponse, String>, RuntimeEffectControllerError> {
        match self {
            Self::ExecCode { result } => Ok(*result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ExecCode,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_checkpoint(
        self,
    ) -> Result<
        (
            CheckpointOutcome,
            Vec<crate::QueuedWorkClaim>,
            Option<crate::TurnInputClaim>,
        ),
        RuntimeEffectControllerError,
    > {
        match self {
            Self::Checkpoint { result, claims } => {
                Ok((result, claims.queued_work_claims, claims.turn_input_claim))
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Checkpoint,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_sync_execution_environment(
        self,
    ) -> Result<Result<Option<ExecutionEnvironmentSync>, String>, RuntimeEffectControllerError>
    {
        match self {
            Self::SyncExecutionEnvironment { result } => Ok(result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::SyncExecutionEnvironment,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_await_event(
        self,
    ) -> Result<crate::Resolution, RuntimeEffectControllerError> {
        match self {
            Self::AwaitEvent { resolution } => Ok(resolution),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AwaitEvent,
                other.kind(),
            )),
        }
    }

    /// Extracts the peek await event outcome for effect-host implementors while executing or
    /// replaying a runtime effect.
    pub fn into_peek_await_event(
        self,
    ) -> Result<Option<crate::Resolution>, RuntimeEffectControllerError> {
        match self {
            Self::PeekAwaitEvent { resolution } => Ok(resolution),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::PeekAwaitEvent,
                other.kind(),
            )),
        }
    }

    /// Exposes kind to effect-host implementors while executing or replaying a runtime effect.
    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,
            Self::ToolBatch { .. } => RuntimeEffectKind::ToolBatch,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::Sleep => RuntimeEffectKind::Sleep,
            Self::AwaitEvent { .. } => RuntimeEffectKind::AwaitEvent,
            Self::PeekAwaitEvent { .. } => RuntimeEffectKind::PeekAwaitEvent,
        }
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    fn invocation(kind: RuntimeEffectKind) -> RuntimeInvocation {
        RuntimeInvocation::effect(RuntimeScope::new("session"), "effect", kind, "replay")
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn process_transfer_v1_identity_golden() {
        let process_ids = vec![
            "process:a:b".to_string(),
            "process\0b".to_string(),
            "λ".to_string(),
        ];
        assert_eq!(
            hex(&process_transfer_set_preimage(&process_ids)),
            "6c6173682d737461626c652d6964656e74697479010100000000000000196c6173682e70726f636573732d7472616e736665722d7365740000000000000003000000000000000b70726f636573733a613a62000000000000000970726f6365737300620000000000000002cebb"
        );
        assert_eq!(
            process_transfer_set_identity(&process_ids),
            "process-transfer-set:v1:sha256:cbbb28ff8d8ab022f3f9f625ebaeb7413f1e4da075f32010892e60200db64781"
        );
    }

    fn prepared_call(call_id: &str) -> crate::PreparedToolCall {
        crate::PreparedToolCall::from_parts(
            call_id,
            "tool:test",
            "test",
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        )
    }

    fn attempt(call_id: &str, attempt: u32, max_attempts: u32) -> RuntimeEffectCommand {
        RuntimeEffectCommand::ToolAttempt {
            call: prepared_call(call_id),
            execution_grant: None,
            attempt,
            max_attempts,
        }
    }

    fn batch() -> crate::PreparedToolBatch {
        crate::PreparedToolBatch::new("batch", vec![prepared_call("call")])
    }

    fn assert_rejected(
        invocation: RuntimeInvocation,
        command: RuntimeEffectCommand,
        expected_code: &str,
    ) {
        let error = RuntimeEffectEnvelope::try_new(invocation, command)
            .expect_err("invalid envelope must be rejected");
        assert_eq!(error.code.as_str(), expected_code);
    }

    #[test]
    fn rejects_non_effect_subject() {
        let mut value = invocation(RuntimeEffectKind::Sleep);
        value.subject = RuntimeSubject::Process {
            process_id: "process".to_string(),
        };
        assert_rejected(
            value,
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
            "runtime_effect_invocation_subject",
        );
    }

    #[test]
    fn rejects_empty_effect_id() {
        assert_rejected(
            RuntimeInvocation::effect(
                RuntimeScope::new("session"),
                "  ",
                RuntimeEffectKind::Sleep,
                "replay",
            ),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
            "runtime_effect_invocation_subject",
        );
    }

    #[test]
    fn rejects_invocation_command_kind_mismatch() {
        assert_rejected(
            invocation(RuntimeEffectKind::AwaitEvent),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
            "runtime_effect_invocation_kind",
        );
    }

    #[test]
    fn rejects_missing_or_empty_replay_key() {
        for replay in [None, Some(RuntimeReplay { key: String::new() })] {
            let mut value = invocation(RuntimeEffectKind::Sleep);
            value.replay = replay;
            assert_rejected(
                value,
                RuntimeEffectCommand::Sleep { duration_ms: 1 },
                "runtime_effect_replay_required",
            );
        }
    }

    #[test]
    fn rejects_empty_tool_attempt_call_id() {
        assert_rejected(
            invocation(RuntimeEffectKind::ToolAttempt),
            attempt(" ", 1, 1),
            "runtime_effect_tool_attempt_call_id",
        );
    }

    #[test]
    fn rejects_tool_attempt_indices_outside_one_through_max() {
        for (attempt_index, max_attempts) in [(0, 1), (1, 0), (2, 1)] {
            assert_rejected(
                invocation(RuntimeEffectKind::ToolAttempt),
                attempt("call", attempt_index, max_attempts),
                "runtime_effect_tool_attempt_index",
            );
        }
    }

    #[test]
    fn rejects_empty_tool_batch_id() {
        let mut value = batch();
        value.batch_id = " ".to_string();
        assert_rejected(
            invocation(RuntimeEffectKind::ToolBatch),
            RuntimeEffectCommand::ToolBatch { batch: value },
            "runtime_effect_tool_batch_id",
        );
    }

    #[test]
    fn rejects_empty_tool_batch() {
        let mut value = batch();
        value.calls.clear();
        assert_rejected(
            invocation(RuntimeEffectKind::ToolBatch),
            RuntimeEffectCommand::ToolBatch { batch: value },
            "runtime_effect_tool_batch_empty",
        );
    }

    #[test]
    fn rejects_empty_tool_batch_child_call_id() {
        let mut value = batch();
        value.calls[0].call.call_id = " ".to_string();
        assert_rejected(
            invocation(RuntimeEffectKind::ToolBatch),
            RuntimeEffectCommand::ToolBatch { batch: value },
            "runtime_effect_tool_batch_call_id",
        );
    }

    #[test]
    fn rejects_empty_tool_batch_child_replay_suffix() {
        let mut value = batch();
        value.calls[0].replay_suffix = " ".to_string();
        assert_rejected(
            invocation(RuntimeEffectKind::ToolBatch),
            RuntimeEffectCommand::ToolBatch { batch: value },
            "runtime_effect_tool_batch_call_replay",
        );
    }
}
