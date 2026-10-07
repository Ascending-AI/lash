use crate::ProcessId;
use crate::SessionId;
pub use lash_core_store::effect_identity::*;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::CheckpointKind;
use crate::llm::types::{
    AttachmentSource, LlmEventSender, LlmMessage, LlmOutputSpec, LlmProviderTraceSender,
    LlmToolChoice, LlmToolSpec,
};
use crate::sansio::{ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure, LlmCallError};
use crate::tool_dispatch::ToolTriggerEffectOutcome;
use crate::{
    AttachmentCreateMeta, CausalRef, CheckpointDelivery, EffectAddress, ExecResponse,
    ExecutionScope, LlmRequest as CoreLlmRequest, LlmResponse, ProcessAwaitOutput,
    ProcessExecutionContext, ProcessListMode, ProcessRecord, ProcessStartRegistration,
    SessionScope,
};

use super::executor::RuntimeEffectControllerError;
use super::llm_outcome::{AssistantResponsePlan, AssistantStreamHookState, LlmStreamRecord};

/// Effect-specific header whose address is present by construction.
///
/// Unlike [`RuntimeInvocation`], this cannot represent a process, trigger, or
/// session-node subject and cannot carry a second optional replay key. The
/// descriptive `effect_id` does not participate in journal identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeEffectInvocation {
    pub address: EffectAddress,
    pub effect_id: String,
    #[serde(default, skip_serializing_if = "RuntimeAttribution::is_none")]
    pub attribution: RuntimeAttribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_attribution: Option<RuntimeReplayAttribution>,
}

impl RuntimeEffectInvocation {
    #[expect(
        clippy::expect_used,
        reason = "the panicking form of `try_new`, kept for implementors"
    )]
    pub fn new(
        address: EffectAddress,
        attribution: RuntimeAttribution,
        effect_id: impl Into<String>,
    ) -> Self {
        Self::try_new(address, attribution, effect_id).expect("valid runtime effect invocation")
    }

    pub fn try_new(
        address: EffectAddress,
        attribution: RuntimeAttribution,
        effect_id: impl Into<String>,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let invocation = Self {
            address,
            effect_id: effect_id.into(),
            attribution,
            caused_by: None,
            replay_attribution: None,
        };
        invocation.validate()?;
        Ok(invocation)
    }

    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.effect_id.trim().is_empty() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
                "runtime effect envelope effect id must be non-empty",
            ));
        }
        self.address.validate().map_err(|error| {
            let code = match error {
                lash_sansio::EffectIdentityError::MissingExecutionScopeId => {
                    crate::RuntimeErrorCode::MissingExecutionScopeId
                }
                lash_sansio::EffectIdentityError::MissingReplayKey => {
                    crate::RuntimeErrorCode::RuntimeEffectReplayRequired
                }
            };
            RuntimeEffectControllerError::new(code, error.to_string())
        })?;
        self.attribution.validate()
    }

    #[must_use]
    pub fn with_caused_by(mut self, caused_by: Option<CausalRef>) -> Self {
        self.caused_by = caused_by;
        self
    }

    #[must_use]
    pub fn with_replay_attribution(mut self, attribution: RuntimeReplayAttribution) -> Self {
        self.replay_attribution = Some(attribution);
        self
    }

    pub fn into_runtime_invocation(self) -> RuntimeInvocation {
        RuntimeInvocation {
            attribution: self.attribution,
            subject: RuntimeSubject::Effect {
                address: self.address,
                effect_id: self.effect_id,
                replay_attribution: self.replay_attribution,
            },
            caused_by: self.caused_by,
            replay: None,
        }
    }

    pub fn address(&self) -> &EffectAddress {
        &self.address
    }

    pub fn effect_id(&self) -> &str {
        &self.effect_id
    }

    pub fn effect_replay_key(&self) -> &str {
        &self.address.replay_key
    }

    pub fn replay_attribution(&self) -> Option<&RuntimeReplayAttribution> {
        self.replay_attribution.as_ref()
    }

    pub fn causal_ref(&self) -> CausalRef {
        CausalRef::Effect {
            address: self.address.clone(),
        }
    }

    pub fn execution_scope(&self) -> &ExecutionScope {
        &self.address().execution_scope
    }

    /// Proves that this invocation belongs to the controller scope which is
    /// about to admit it. Callers use this before capture, storage, or local
    /// execution so a mismatched address cannot create partial durable state.
    pub fn validate_execution_scope(
        &self,
        admitted_scope: &ExecutionScope,
    ) -> Result<(), RuntimeEffectControllerError> {
        if self.execution_scope() == admitted_scope {
            return Ok(());
        }
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectScopeMismatch,
            format!(
                "effect address scope {:?} does not match admitted controller scope {:?}",
                self.execution_scope(),
                admitted_scope
            ),
        ))
    }
}

impl<'de> Deserialize<'de> for RuntimeEffectInvocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            address: EffectAddress,
            effect_id: String,
            #[serde(default)]
            attribution: RuntimeAttribution,
            #[serde(default)]
            caused_by: Option<CausalRef>,
            #[serde(default)]
            replay_attribution: Option<RuntimeReplayAttribution>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut invocation = Self::try_new(wire.address, wire.attribution, wire.effect_id)
            .map_err(serde::de::Error::custom)?;
        invocation.caused_by = wire.caused_by;
        invocation.replay_attribution = wire.replay_attribution;
        Ok(invocation)
    }
}

/// Fully serializable envelope emitted at Lash's nondeterministic boundary.
///
/// Decoding validates the invocation and command as construction does.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "RuntimeEffectEnvelopeWire")]
pub struct RuntimeEffectEnvelope {
    pub invocation: RuntimeEffectInvocation,
    pub command: RuntimeEffectCommand,
}

#[derive(Deserialize)]
struct RuntimeEffectEnvelopeWire {
    invocation: RuntimeEffectInvocation,
    command: RuntimeEffectCommand,
}

impl TryFrom<RuntimeEffectEnvelopeWire> for RuntimeEffectEnvelope {
    type Error = RuntimeEffectControllerError;

    fn try_from(wire: RuntimeEffectEnvelopeWire) -> Result<Self, Self::Error> {
        let envelope = Self::try_new(wire.invocation, wire.command)?;
        Ok(envelope)
    }
}

// Measured 600 B on rustc 1.97.0, x86_64-unknown-linux-gnu after the
// endpoint-aware replay route and typed tool-intent attribution cutovers.
const _: () = assert!(std::mem::size_of::<RuntimeEffectEnvelope>() <= 1024);

impl RuntimeEffectEnvelope {
    /// Constructs a validated effect envelope for effect-host implementors and panics if the
    /// invocation and command violate the durable-effect contract.
    #[expect(
        clippy::expect_used,
        reason = "the panicking form of `try_new`, kept for implementors"
    )]
    pub fn new(invocation: RuntimeEffectInvocation, command: RuntimeEffectCommand) -> Self {
        Self::try_new(invocation, command).expect("valid runtime effect invocation")
    }

    /// The admitted address and descriptive effect label must be valid, and tool attempts and
    /// batches must carry valid indices and IDs.
    pub fn try_new(
        invocation: RuntimeEffectInvocation,
        command: RuntimeEffectCommand,
    ) -> Result<Self, RuntimeEffectControllerError> {
        invocation.validate()?;
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

impl RuntimeEffectCommand {
    /// This command without the trace provenance it carries, when it
    /// carries any: the business projection the envelope's canonical form
    /// is taken over. A trace cause, offer or scope sits beside a command's
    /// payload for the store that admits it; it is never part of what a
    /// replay compares.
    pub fn without_trace_provenance(&self) -> Option<Self> {
        match self {
            Self::AcceptTurnInput { draft } if !draft.trace_cause.is_root() => {
                let mut draft = draft.clone();
                draft.trace_cause = lash_trace::TraceCause::Root;
                Some(Self::AcceptTurnInput { draft })
            }
            Self::Process { command } => {
                command
                    .without_trace_provenance()
                    .map(|command| Self::Process {
                        command: Box::new(command),
                    })
            }
            _ => None,
        }
    }
}

impl ProcessCommand {
    /// [`RuntimeEffectCommand::without_trace_provenance`] for a process
    /// command.
    pub fn without_trace_provenance(&self) -> Option<Self> {
        match self {
            Self::Start {
                registration,
                observers,
                execution_context,
            } if !registration.trace.is_empty() => Some(Self::Start {
                registration: registration
                    .clone()
                    .with_trace(lash_trace::TraceScopeOffer::default()),
                observers: observers.clone(),
                execution_context: execution_context.clone(),
            }),
            Self::Signal { signal } if !signal.trace_cause.is_root() => Some(Self::Signal {
                signal: signal
                    .clone()
                    .with_trace_cause(lash_trace::TraceCause::Root),
            }),
            Self::EmitEvent {
                process_id,
                request,
            } if !request.trace_cause.is_root() => {
                let mut request = request.clone();
                request.trace_cause = lash_trace::TraceCause::Root;
                Some(Self::EmitEvent {
                    process_id: process_id.clone(),
                    request,
                })
            }
            _ => None,
        }
    }
}

fn validate_effect_command(
    command: &RuntimeEffectCommand,
) -> Result<(), RuntimeEffectControllerError> {
    if let RuntimeEffectCommand::ToolAttempt {
        call: _,
        execution_grant: _,
        attempt,
        max_attempts,
    } = command
        && (*attempt == 0 || *max_attempts == 0 || *attempt > *max_attempts)
    {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectToolAttemptIndex,
            format!(
                "runtime effect tool attempt must satisfy 1 <= attempt <= max_attempts, got {attempt}/{max_attempts}"
            ),
        ));
    }
    Ok(())
}

/// A sleep's durable intent: relative duration or absolute wall-clock deadline.
///
/// One shape for the whole path — guest bridge, effect envelope, and claim
/// derivation — so adding a sleep shape is a compile error at every consumer
/// instead of a silent fall-through. `Until` carries epoch milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SleepSpec {
    For {
        duration_ms: u64,
    },
    /// Sleep until the wall-clock instant `deadline_ms`.
    Until {
        deadline_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffectCommand {
    TransitionPlugins {
        request: Box<crate::plugin::PluginTransitionRequest>,
    },
    /// Retain a scope start or terminal beside its owning execution boundary.
    TraceBoundary {
        scope: lash_trace::TraceScopeId,
        transition: lash_trace::TraceTransitionKind,
    },
    /// Record the protocol's decision before the paired model call.
    BeforeLlmCall {
        request: Box<CoreLlmRequest>,
    },
    LlmCall {
        request: Box<LlmRequestSpec>,
    },
    /// Run host assistant-response hooks over the raw provider completion that
    /// the paired [`RuntimeEffectCommand::LlmCall`] already journaled.
    ///
    /// The payload is a replay-deterministic derivation of phase 1's journaled
    /// outcome, so this command is reconstructed identically on redrive.
    AssistantResponseHooks {
        response: Box<LlmResponse>,
        /// The exact ordered callback keys and revisions phase 1 recorded.
        plan: AssistantResponsePlan,
        /// The stream-hook end states phase 1 recorded. The response hooks
        /// read these, never state a stream hook left in plugin memory, so
        /// phase 2 derives the same response on any worker.
        stream_hook_states: Vec<AssistantStreamHookState>,
    },
    Direct {
        request: Box<LlmRequestSpec>,
        usage_source: String,
    },
    ToolAttempt {
        call: Box<crate::PreparedToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
    },

    /// The recorded presentation boundary (ADR 0099 §6, FIG-3420): folds the
    /// session's ordered presentation steps over this settled output once and
    /// journals the resulting [`ToolPresentation`](super::ToolPresentation).
    /// Replay serves the recorded outcome and never re-runs a step.
    ///
    /// The command names only recorded facts. How long the call took is an
    /// observation: a redrive serves a journaled attempt at once, so a
    /// duration here would move the envelope of a healthy redrive. The steps still read the
    /// live duration from the local executor, and the outcome they fold to is
    /// what replay serves.
    PresentToolResult {
        /// The recorded presenter and steps, resolved before any callback runs.
        plan: Box<super::PresentationBinding>,
        call_id: crate::ToolCallId,
        tool_id: crate::ToolId,
        tool_name: String,
        render: Option<crate::RecordedRender>,
        args: serde_json::Value,
        output: Box<crate::ToolCallOutput>,
    },
    Trigger {
        command: Box<crate::TriggerCommand>,
    },
    Process {
        command: Box<ProcessCommand>,
    },
    /// Run one code cell against the session's interpreter.
    ///
    /// Never journaled on any host: replay re-executes the cell, and every
    /// nested effect the cell issues is journaled and replayed on its own
    /// replay key (ADR 0103). See [`Self::replays_by_reexecution`].
    ExecCode {
        code: String,
    },
    /// Write the Pending Turn Input row that admits a turn (ADR 0069 §1),
    /// through the effect journal so a replaying engine re-derives the
    /// admission rather than re-performing it (ADR 0069 §6).
    AcceptTurnInput {
        draft: Box<crate::PendingTurnInputDraft>,
    },
    /// Record a sequential callback slot's decisions with the resolutions of
    /// the state commands its callbacks returned (K10, FIG-4878). Replay
    /// serves the decisions and publishes the resolutions; no callback or
    /// reducer runs again.
    PluginCallbacks {
        phase: crate::plugin::RecordedCallbackPhase,
    },

    /// Resolve the shape `run` runs under (FIG-3600 S6, FIG-3838): once per
    /// run, keyed by it, so every redrive replays the recorded shape.
    ResolveTurnConfig {
        run: crate::TurnId,
    },
    /// Record the base an administrative compaction (`compact_context`)
    /// summarizes and opens its frame from (FIG-4133): the head and the frame
    /// current when it starts, before its summarizer runs. Keyed by the
    /// compaction's ordinal in its run, so a redrive replays the base its
    /// first execution recorded, even after the compaction's own commit moved
    /// the head, and a repeated compaction records a base of its own. The
    /// envelope names only the session: the base is the step's outcome.
    RecordCompactionBase {
        session: crate::SessionId,
    },
    /// Render the system prompt a compaction's summarizer call carries, as
    /// one recorded step before that call (FIG-4589). The protocol plugin
    /// renders it from the session's recorded config; the text is the step's
    /// outcome, so a redrive serves it and never renders again. Keyed by the
    /// compaction's ordinal in its scope. The envelope names only the
    /// session.
    RenderCompactionPrompt {
        session: crate::SessionId,
    },
    /// Resolve a config transaction once, before anything publishes
    /// (FIG-4379): the base revision it resolved against and either the
    /// complete replacements with each command's output, a stale base, or a
    /// typed refusal. Keyed by the transaction's command, so a redrive
    /// publishes the recorded resolution and never runs a reducer again. The
    /// envelope names only the session and the transaction id: the
    /// resolution is the step's outcome.
    ResolveConfigTransaction {
        session: crate::SessionId,
        transaction: String,
    },
    /// Read the session's leading open command run for a command run to
    /// apply (ADR 0101 §4, FIG-4201), acknowledging its obligations
    /// delivered under the run's fence. Keyed by the read's ordinal in the
    /// run, so a redrive of the run reads back the execution its first execution
    /// applied at each ordinal and applies it again, replaying the steps an
    /// administrative compaction journaled and meeting the receipts of the
    /// commits that landed, even after those commits settled the lane. The
    /// envelope names only the session: the run is the step's outcome.
    ReadSessionCommandRun {
        session: crate::SessionId,
    },
    /// Close a logical run's scope after its terminal evidence (FIG-3600
    /// S7, FIG-3607 item 7). Recorded under the run's scope at
    /// [`shift_close_run_replay_key`](crate::engine::shift_close_run_replay_key),
    /// so a crash between the run's terminal commit and its close re-runs
    /// the close; the session is the scope's.
    CloseRunScope {
        run: crate::TurnId,
    },
    Checkpoint {
        checkpoint: CheckpointKind,
    },
    /// Build and journal the environment the next protocol iteration's model
    /// call runs under (FIG-3538); every sync carries it, the protocol-start
    /// one included (FIG-3587).
    SyncExecutionEnvironment,
    /// Validate and hold the recorded process execution environment.
    /// The recorded outcome carries its digest,
    /// whose immutable bytes the execution referrer keeps available to replay.
    ///
    /// The outcome is that reference, or the refusal of an environment the
    /// store holds but this build cannot reconstruct.
    /// A store that did not answer is a fault of this attempt, never the
    /// step's outcome: the executor marks it retryable (see
    /// [`RuntimeEffectControllerError::retryable_uncommitted_derivation`]),
    /// and an engine runs the step again rather than recording it.
    LoadExecutionEnv {
        env: crate::ProcessExecutionEnvRef,
    },
    /// Sleep for a relative duration or until an absolute wall-clock deadline.
    ///
    /// The intent is the journaled parameter, never a duration derived from the
    /// clock at execution time, so a deadline-bearing sleep replays against the
    /// same envelope (FIG-2968).
    Sleep {
        spec: SleepSpec,
    },
    LanguageRuntimeValue {
        operation: String,
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

    /// Whether every host answers this command by running the local executor
    /// again on replay instead of serving a recorded outcome (ADR 0103).
    ///
    /// True for [`ExecCode`](Self::ExecCode) only. A code cell's outcome is
    /// not the whole of its effect: the cell also mutates interpreter globals
    /// and deferred-resolution records, which the turn's final commit
    /// snapshots. A recorded `ExecResponse` cannot rebuild that state, so the
    /// cell resumes from its VM snapshot (ADR 0132 §8) rather than from a
    /// recorded response, and the host skips the claim.
    pub fn replays_by_reexecution(&self) -> bool {
        matches!(self, Self::ExecCode { .. })
    }

    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::TransitionPlugins { .. } => RuntimeEffectKind::TransitionPlugins,
            Self::BeforeLlmCall { .. } => RuntimeEffectKind::BeforeLlmCall,
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::AssistantResponseHooks { .. } => RuntimeEffectKind::AssistantResponseHooks,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,

            Self::PresentToolResult { .. } => RuntimeEffectKind::PresentToolResult,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::AcceptTurnInput { .. } => RuntimeEffectKind::AcceptTurnInput,
            Self::PluginCallbacks { .. } => RuntimeEffectKind::PluginCallbacks,

            Self::TraceBoundary { .. } => RuntimeEffectKind::TraceBoundary,
            Self::ResolveTurnConfig { .. } => RuntimeEffectKind::ResolveTurnConfig,
            Self::RecordCompactionBase { .. } => RuntimeEffectKind::RecordCompactionBase,
            Self::RenderCompactionPrompt { .. } => RuntimeEffectKind::RenderCompactionPrompt,
            Self::ResolveConfigTransaction { .. } => RuntimeEffectKind::ResolveConfigTransaction,
            Self::ReadSessionCommandRun { .. } => RuntimeEffectKind::ReadSessionCommandRun,
            Self::CloseRunScope { .. } => RuntimeEffectKind::CloseRunScope,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::LoadExecutionEnv { .. } => RuntimeEffectKind::LoadExecutionEnv,
            Self::Sleep { .. } => RuntimeEffectKind::Sleep,
            Self::LanguageRuntimeValue { .. } => RuntimeEffectKind::LanguageRuntimeValue,
        }
    }
}

/// The scope and status selection recorded by a process listing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessListSelection {
    Observed {
        session_scope: SessionScope,
        mode: ProcessListMode,
    },
    HostRunning,
}

/// Serializable operation against the process admin plane.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// justification: RuntimeEffectCommand already boxes every ProcessCommand, so boxing its Start payload again is redundant.
#[allow(clippy::large_enum_variant)]
pub enum ProcessCommand {
    Start {
        registration: ProcessStartRegistration,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        observers: Vec<SessionId>,
        #[serde(
            default,
            skip_serializing_if = "boxed_process_execution_context_is_empty"
        )]
        execution_context: Box<ProcessExecutionContext>,
    },
    List {
        selection: ProcessListSelection,
    },
    ValidateVisible {
        owner: crate::RuntimeOwner,
        process_ids: Vec<ProcessId>,
    },
    Transfer {
        from_scope: SessionScope,
        to_scope: SessionScope,
        process_ids: Vec<ProcessId>,
    },
    DeleteSession {
        session_id: SessionId,
    },
    Await {
        process_id: ProcessId,
    },
    Cancel {
        process_id: ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribution: Option<crate::RuntimeReplayAttribution>,
    },
    /// Deliver one signal. The command carries the signal as it is
    /// admitted, never an append request: the append key is derived from the
    /// signal's identity at admission, so no caller can select another
    /// (FIG-4299).
    Signal {
        signal: crate::ProcessSignal,
    },
    EmitEvent {
        process_id: ProcessId,
        request: crate::ProcessEventAppendRequest,
    },
    /// The journaled immutable-definition publish: the descriptor write and
    /// the referrer edges of its artifact closure cross the runtime-effect
    /// seam like every other journaled admission, so a redrive replays the
    /// recorded definition instead of writing a second one (ADR 0113 §3.6).
    PublishDefinition {
        draft: crate::ProcessDefinitionDraft,
        module: Option<crate::DeclaredModuleArtifact>,
    },
    GetDefinition {
        definition_id: crate::ProcessDefinitionId,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
// justification: the decode shape mirrors ProcessCommand, whose Start payload is not boxed for the same reason.
#[allow(clippy::large_enum_variant)]
enum ProcessCommandDecode {
    Start {
        registration: ProcessStartRegistration,
        #[serde(default)]
        observers: Vec<SessionId>,
        #[serde(default)]
        execution_context: Box<ProcessExecutionContext>,
    },
    List {
        selection: ProcessListSelection,
    },
    ValidateVisible {
        owner: crate::RuntimeOwner,
        process_ids: Vec<ProcessId>,
    },
    Transfer {
        from_scope: SessionScope,
        to_scope: SessionScope,
        process_ids: Vec<ProcessId>,
    },
    DeleteSession {
        session_id: SessionId,
    },
    Await {
        process_id: ProcessId,
    },
    Cancel {
        process_id: ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        #[serde(default)]
        attribution: Option<crate::RuntimeReplayAttribution>,
    },
    Signal {
        signal: crate::ProcessSignal,
    },
    EmitEvent {
        process_id: ProcessId,
        request: crate::ProcessEventAppendRequest,
    },
    PublishDefinition {
        draft: crate::ProcessDefinitionDraft,
        module: Option<crate::DeclaredModuleArtifact>,
    },
    GetDefinition {
        definition_id: crate::ProcessDefinitionId,
    },
}

impl<'de> Deserialize<'de> for ProcessCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(object) = value.as_object()
            && object.contains_key("process_ref")
        {
            return Err(serde::de::Error::custom(
                "process_reference_format_cutover: an incarnation-bearing process command cannot be replayed because a process is now named by its minted process id",
            ));
        }
        let decoded: ProcessCommandDecode =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(match decoded {
            ProcessCommandDecode::Start {
                registration,
                observers,
                execution_context,
            } => Self::Start {
                registration,
                observers,
                execution_context,
            },
            ProcessCommandDecode::List { selection } => Self::List { selection },
            ProcessCommandDecode::ValidateVisible { owner, process_ids } => {
                Self::ValidateVisible { owner, process_ids }
            }
            ProcessCommandDecode::Transfer {
                from_scope,
                to_scope,
                process_ids,
            } => Self::Transfer {
                from_scope,
                to_scope,
                process_ids,
            },
            ProcessCommandDecode::DeleteSession { session_id } => {
                Self::DeleteSession { session_id }
            }
            ProcessCommandDecode::Await { process_id } => Self::Await { process_id },
            ProcessCommandDecode::Cancel {
                process_id,
                origin,
                requester,
                attribution,
            } => Self::Cancel {
                process_id,
                origin,
                requester,
                attribution,
            },
            ProcessCommandDecode::Signal { signal } => Self::Signal { signal },
            ProcessCommandDecode::EmitEvent {
                process_id,
                request,
            } => Self::EmitEvent {
                process_id,
                request,
            },
            ProcessCommandDecode::PublishDefinition { draft, module } => {
                Self::PublishDefinition { draft, module }
            }
            ProcessCommandDecode::GetDefinition { definition_id } => {
                Self::GetDefinition { definition_id }
            }
        })
    }
}

fn boxed_process_execution_context_is_empty(context: &ProcessExecutionContext) -> bool {
    context.is_empty()
}

type CheckpointOutcome = Result<CheckpointDelivery, RuntimeEffectControllerError>;

/// What a checkpoint's turn holds once the checkpoint committed (FIG-3927):
/// every queued-work admission the turn made so far, and the active-turn
/// input its checkpoints admitted.
///
/// Checkpoint replay skips the local executor that admitted these rows, so
/// they are journaled with the delivery: the replaying turn settles exactly
/// the rows its run holds in its final commit, and never reads the queue.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointAdmittedSet {
    /// Session changes returned by the checkpoint callbacks. The driver applies
    /// them after this outcome is durable, on the live pass and on replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_contributions: Vec<crate::plugin::SessionContributions>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_work: Vec<crate::AdmittedQueuedWork>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_inputs: Option<crate::AdmittedTurnInputs>,
    /// What the turn's opener had incorporated when this checkpoint committed
    /// (ADR 0099 §6, §13). Replay serves a completed cell from the journal
    /// without re-running the incorporations it made, and the checkpoint that
    /// followed already delivered their facts; the replaying turn restores
    /// this ledger so its end does not incorporate those ranks a second time.
    /// Outcomes written by older binaries carry none: such a turn's groups
    /// predate the opener ledger.
    #[serde(
        default,
        skip_serializing_if = "crate::session::IncorporationLedger::is_empty"
    )]
    pub incorporation: crate::session::IncorporationLedger,
}

/// The replay-key suffix of a parked call's cancel obligation (ADR 0116
/// §3.4): the process cancel a cancelled or timed-out wait owes is journaled
/// beneath `{call id}:cancel-work` under the call's lineage, so every redrive
/// re-issues the same command.
pub fn tool_cancel_work_replay_suffix(call_id: &crate::ToolCallId) -> String {
    crate::runtime::causal::CommandSubKey::ToolCancelWork {
        call_id: call_id.clone(),
    }
    .to_string()
}

impl ProcessCommand {
    /// The effect id of a start under `start_key`: its admitted operation
    /// identity. A journaled start always carries a key; the unkeyed spelling
    /// exists only so the executor can refuse the shape by name.
    pub fn start_effect_id(start_key: Option<&crate::StartKey>) -> String {
        match start_key {
            Some(start_key) => {
                crate::runtime::causal::CommandSubKey::ProcessStart(start_key.as_str()).to_string()
            }
            None => crate::runtime::causal::CommandSubKey::ProcessStart("unkeyed").to_string(),
        }
    }

    /// Derives the stable effect ID process-engine and effect-host implementors use to journal this
    /// process command without conflating command kinds.
    pub fn effect_id(&self) -> String {
        match self {
            // A start is addressed by its key — its admitted operation
            // identity — never by the process id it mints (ADR 0107).
            Self::Start { registration, .. } => {
                Self::start_effect_id(registration.start_key.as_ref())
            }
            Self::List { selection } => match selection {
                ProcessListSelection::Observed {
                    session_scope,
                    mode,
                } => format!("process:list:{}:{}", session_scope.id(), mode.as_str()),
                ProcessListSelection::HostRunning => "process:list:host:running".to_string(),
            },
            Self::ValidateVisible { owner, process_ids } => {
                let digest = process_transfer_set_identity(process_ids);
                format!("process:validate-visible:{owner}:{digest}")
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
            Self::Await { process_id } => {
                crate::runtime::causal::CommandSubKey::ProcessAwait(process_id.as_ref()).to_string()
            }
            Self::Cancel { process_id, .. } => format!("process:cancel:{process_id}"),
            Self::Signal { signal } => format!(
                "process:signal:{}:signal.{}:{}",
                signal.identity.process_id(),
                signal.identity.signal_name(),
                signal.identity.signal_id()
            ),
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
            Self::PublishDefinition { draft, .. } => {
                format!("process:publish-definition:{}", draft.id())
            }
            Self::GetDefinition { definition_id } => {
                format!("process:get-definition:{definition_id}")
            }
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
        /// Whether this start created the process or found it registered.
        disposition: crate::ProcessRegistrationOutcome,
    },
    List {
        entries: Vec<ProcessRecord>,
    },
    ValidateVisible {
        not_visible: Option<ProcessId>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_delivery: Option<Box<crate::ProcessWakeDelivery>>,
    },
    Definition {
        definition: Box<crate::ProcessDefinition>,
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
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolAttemptLaunch {
    Done {
        record: Box<crate::ToolCallRecord>,
        intents: crate::ToolIntents,
    },
}

/// Plugin-attributed runtime events emitted by one assistant-response hook.
///
/// Journaled with phase 2's outcome so replay serves the events at their
/// original placement instead of re-running the hook that produced them, and
/// never folds them into the phase-1 provider-completion entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssistantResponseHookEvents {
    pub plugin_id: String,
    pub events: Vec<crate::PluginRuntimeEvent>,
}

pub type RuntimeAssistantResponseHooksOutcome = (LlmResponse, Vec<AssistantResponseHookEvents>);

pub type RuntimeDirectLlmOutcome = (
    Result<LlmResponse, LlmCallError>,
    Option<crate::LlmCallRecord>,
);

/// The base an administrative compaction records before its summarizer
/// runs (FIG-4133): the durable head it summarizes and the frame it opens its
/// frame from. The compaction commits under the fence of the command run
/// that applies it (FIG-4201), so the base records no fence. Plain store
/// identities, so any build replays it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionBase {
    pub head: crate::store::SessionHeadRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<crate::FrameNodeId>,
}

/// Serializable result of a runtime effect command.
///
/// Large payloads stay boxed so this boundary type remains cheap to retain in
/// nested async controller frames. `Box<T>` is serde-transparent, so durable
/// journal records keep their established JSON shape and full-record evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffectOutcome {
    TransitionPlugins {
        record: Box<crate::plugin::PluginTransitionRecord>,
    },
    /// Accepted namespace mutations and the result of the same callback body.
    /// Replay restores the mutations before serving the result.
    PluginState {
        kind: RuntimeEffectKind,
        state: Box<crate::plugin::PluginStateEffect>,
        result: Box<Result<RuntimeEffectOutcome, RuntimeEffectControllerError>>,
    },
    TraceBoundary {
        scope: Box<lash_trace::DurableTraceScope>,
        at_ms: u64,
    },
    BeforeLlmCall {
        decision: Result<Option<crate::ProtocolLlmCallAction>, crate::PluginError>,
    },
    LlmCall {
        result: Box<Result<LlmResponse, LlmCallError>>,
        text_streamed: bool,
        /// Sealed provider-attempt history. Calls interrupted before the
        /// provider handle returns have no record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_record: Option<crate::LlmCallRecord>,
        /// What the provider stream left that later steps read.
        stream: Box<LlmStreamRecord>,
    },
    /// Phase 2 of the staged LLM-call boundary.
    ///
    /// Holds the transformed response host hooks derived from the raw
    /// completion phase 1 journaled, and the events those hooks emitted. Only a
    /// *complete* derivation is journaled: a failing hook fails the phase
    /// instead, so a crash or hook failure between the two phases redrives this
    /// entry alone and the paid completion is replayed, never re-bought.
    AssistantResponseHooks {
        response: Box<LlmResponse>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        events: Vec<AssistantResponseHookEvents>,
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

    /// What the [`PresentToolResult`](RuntimeEffectCommand::PresentToolResult)
    /// boundary journaled: the folded `ModelToolReturn` plus every artifact a
    /// step retained while the chain ran. Replay serves this record verbatim —
    /// presentation steps never re-run (ADR 0099 §6, FIG-3420).
    PresentToolResult {
        presentation: Box<super::ToolPresentation>,
    },
    Trigger {
        result: Box<crate::TriggerEffectResult>,
    },
    Process {
        result: ProcessEffectOutcome,
    },
    ExecCode {
        result: Box<Result<ExecResponse, crate::ExecCodeFailure>>,
    },
    /// The admitted Pending Turn Input row, journaled so replay returns the
    /// same acceptance identity the first execution minted.
    AcceptTurnInput {
        accepted: Box<crate::PendingTurnInput>,
    },
    /// A sequential callback slot's recorded decisions, or the failure of
    /// one of its callbacks, which publishes none of the slot's commands.
    PluginCallbacks {
        result: Result<Vec<crate::plugin::RecordedTurnContribution>, crate::PluginError>,
    },

    /// The run's recorded shape: its spec resolved against its snapshot
    /// of the durable head's config (FIG-3838).
    ResolveTurnConfig {
        resolved: Box<crate::ResolvedRun>,
    },
    /// The base an administrative compaction recorded before its summarizer
    /// ran.
    RecordCompactionBase {
        base: Box<CompactionBase>,
    },
    /// The system prompt a compaction's summarizer call carries, as its
    /// protocol plugin rendered it. `None` when the render is empty.
    RenderCompactionPrompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<std::sync::Arc<str>>,
    },
    /// A config transaction's recorded resolution.
    ResolveConfigTransaction {
        resolution: Box<crate::ConfigResolution>,
    },
    /// The command run a command run read: the leading open batches, in
    /// `enqueue_seq` order, empty when the lane was.
    ReadSessionCommandRun {
        batches: Vec<crate::QueuedWorkBatch>,
    },
    /// The run's scope close was delivered after its terminal evidence.
    CloseRunScope,
    Checkpoint {
        result: CheckpointOutcome,
        #[serde(default)]
        admitted: Box<CheckpointAdmittedSet>,
    },
    SyncExecutionEnvironment {
        /// The digest of the prelude the sync's body wrote to the store set
        /// before this outcome completed (FIG-5133): the journal holds the
        /// reference, never the transcript.
        prelude: TurnPreludeRef,
        result: Box<Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure>>,
        /// The tool surface the sync built: every tool of the catalog the
        /// iteration's calls resolve against, as its definition. The shift
        /// installs it as the catalog, on the live pass and on every replay,
        /// and judges each tool against the live registry on its own (FIG-3672
        /// P7b). Empty when the sync failed.
        tool_surface: Vec<crate::ToolDefinition>,
    },
    /// The environment a load validated and acquired under its execution.
    /// Replay resolves the same immutable bytes from this recorded digest.
    LoadExecutionEnv {
        env: crate::ProcessExecutionEnvRef,
    },
    Sleep,
    LanguageRuntimeValue {
        value: serde_json::Value,
    },
}

// Measured 96 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<RuntimeEffectOutcome>() <= 128);

// =============================================================================
// Request specs (serializable forms of LLM/Direct requests)
// =============================================================================

/// Serializable LLM request data. Live stream and provider-trace callbacks are
/// attached by the local executor, and attachment bytes are resolved locally
/// from refs rather than persisted in the effect envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmRequestSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Arc<str>>,
    pub model: lash_sansio::llm_profile::LlmProfileConfig,
    pub messages: Vec<LlmMessage>,
    pub tools: Arc<Vec<LlmToolSpec>>,
    pub tool_choice: LlmToolChoice,
    /// The session's recorded attachment-acceptance rules the request
    /// renders its attachments under.
    #[serde(
        default,
        skip_serializing_if = "crate::provider::AttachmentCapabilitySnapshot::is_empty_arc"
    )]
    pub attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    #[serde(default)]
    pub generation: crate::GenerationOptions,
    pub scope: crate::LlmRequestScope,
    pub output_spec: Option<LlmOutputSpec>,
}

impl LlmRequestSpec {
    /// Sources are retained by their message blocks.
    pub fn attachments(&self) -> Vec<&AttachmentSource> {
        self.messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .flat_map(crate::llm::types::LlmContentBlock::attachment_sources)
            .collect()
    }

    pub async fn from_request(
        request: &CoreLlmRequest,
        attachment_store: &crate::RuntimeAttachmentStore,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let mut messages = request.messages.clone();
        for message in &mut messages {
            if message
                .blocks
                .iter()
                .flat_map(crate::llm::types::LlmContentBlock::attachment_sources)
                .next()
                .is_none()
            {
                continue;
            }
            for block in Arc::make_mut(&mut message.blocks) {
                match block {
                    crate::llm::types::LlmContentBlock::Attachment { source } => {
                        **source = durable_attachment_source(source, attachment_store).await?;
                    }
                    crate::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                        for part in content {
                            if let crate::ModelToolReturnPart::Attachment(source) = part {
                                *source =
                                    durable_attachment_source(source, attachment_store).await?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            instructions: request.instructions.clone(),
            model: request.model.clone(),
            messages,
            tools: Arc::clone(&request.tools),
            tool_choice: request.tool_choice.clone(),
            attachment_acceptance: Arc::clone(&request.attachment_acceptance),
            generation: request.generation.clone(),
            scope: request.scope.clone(),
            output_spec: request.output_spec.clone(),
        })
    }

    pub fn into_request(
        self,
        stream_events: Option<LlmEventSender>,
        provider_trace: Option<LlmProviderTraceSender>,
    ) -> CoreLlmRequest {
        CoreLlmRequest {
            instructions: self.instructions,
            model: self.model,
            messages: self.messages,
            resolved_stored: Default::default(),
            tools: self.tools,
            tool_choice: self.tool_choice,
            attachment_acceptance: self.attachment_acceptance,
            generation: self.generation,
            scope: self.scope,
            output_spec: self.output_spec,
            stream_events,
            provider_trace,
        }
    }
}

async fn durable_attachment_source(
    attachment: &AttachmentSource,
    attachment_store: &crate::RuntimeAttachmentStore,
) -> Result<AttachmentSource, RuntimeEffectControllerError> {
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
    Ok(source)
}

#[path = "envelope_outcomes.rs"]
mod outcomes;
pub use outcomes::ServedExecutionEnvironmentSync;
#[path = "turn_prelude.rs"]
mod turn_prelude;
pub use turn_prelude::{TurnPrelude, TurnPreludeRef, TurnPreludeStore};

impl From<RuntimeEffectInvocation> for crate::RuntimeInvocation {
    fn from(invocation: RuntimeEffectInvocation) -> Self {
        invocation.into_runtime_invocation()
    }
}

#[cfg(test)]
#[path = "envelope_rejection_tests.rs"]
mod rejection_tests;
