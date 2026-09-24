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
use crate::sansio::{ExecutionEnvironmentSync, LlmCallError};
use crate::tool_dispatch::ToolTriggerEffectOutcome;
use crate::{
    AttachmentCreateMeta, CausalRef, CheckpointDelivery, EffectAddress, ExecResponse,
    ExecutionScope, LlmRequest as CoreLlmRequest, LlmResponse, ProcessAwaitOutput,
    ProcessExecutionContext, ProcessListMode, ProcessRecord, ProcessRegistration, SessionScope,
};

use super::executor::RuntimeEffectControllerError;
use super::group::{EffectGroupMembership, GroupWakePolicy, LoserPolicy};
use super::tool_settlement::{ToolAttemptCapture, ToolSettlement};

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

    pub fn replay_key(&self) -> &str {
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeEffectEnvelope {
    pub invocation: RuntimeEffectInvocation,
    pub command: RuntimeEffectCommand,
    /// This effect's membership in a durable effect group, when it is a group
    /// child (FIG-1416).
    ///
    /// Optional and **omitted when absent**, so an ungrouped effect's canonical
    /// encoding stays byte-identical to what it was before groups existed and
    /// no pre-existing recorded `envelope_hash` is invalidated. Boxed to keep
    /// the envelope inside its measured size budget below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Box<EffectGroupMembership>>,
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
            group: None,
        })
    }

    /// Prefer
    /// [`RuntimeEffectGroup::try_new`](super::group::RuntimeEffectGroup::try_new),
    /// which stamps every child from its own index and checks agreement; reach
    /// for this only to build a child whose membership you then hand to that
    /// constructor for validation.
    ///
    /// The membership folds into [`stable_hash`](Self::stable_hash), so a replay
    /// whose wake rule, loser disposition, or position drifted is refused by the
    /// existing envelope-hash fence rather than executed under the new rule.
    #[must_use]
    pub fn in_effect_group(
        mut self,
        group_key: impl Into<String>,
        position: usize,
        wake: GroupWakePolicy,
        loser_disposition: LoserPolicy,
    ) -> Self {
        self.group = Some(Box::new(EffectGroupMembership {
            group_key: group_key.into(),
            position,
            wake,
            loser_disposition,
        }));
        self
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
    if let RuntimeEffectCommand::PresentToolResult { call_id, .. } = command
        && call_id.trim().is_empty()
    {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectToolAttemptCallId,
            "runtime effect tool presentation requires a non-empty call id",
        ));
    }
    if let RuntimeEffectCommand::ToolInvocation { request } = command {
        request.validate()?;
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

/// Serializable command emitted at Lash's nondeterministic runtime boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffectCommand {
    LlmCall {
        /// The provider the turn's policy names for this call. With the
        /// request's model it is the recorded policy the call runs under, so a
        /// replay whose live policy names another provider diverges instead of
        /// continuing on it.
        provider_id: String,
        request: Box<LlmRequestSpec>,
    },
    /// Run host assistant-response hooks over the raw provider completion that
    /// the paired [`RuntimeEffectCommand::LlmCall`] already journaled.
    ///
    /// The payload is a replay-deterministic derivation of phase 1's journaled
    /// outcome, so this command is reconstructed identically on redrive.
    AssistantResponseHooks {
        response: Box<LlmResponse>,
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
        call: crate::PreparedToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
    },
    /// One tool child of a durable effect group, at invocation level
    /// (ADR 0099 §2, §3).
    ///
    /// [`ToolAttempt`](Self::ToolAttempt) does not name this: it is the atomic
    /// body of a single attempt — the thing that runs inside a recorded body —
    /// so it cannot carry retry, which is a second attempt with a second
    /// envelope hash. The payload is the request that reconstructs the child
    /// from the journal alone, which is what makes an accepted group's
    /// membership recoverable (W1, W2).
    ///
    /// Boxed to keep the command inside its measured size budget below.
    ToolInvocation {
        request: Box<super::tool_child::ToolChildRequest>,
    },
    /// Record the opener's incorporated settlement prefix of a durable effect
    /// group (ADR 0099 §6): the journaled mapping from group identity to the
    /// ranks the opener applied, written before an externally effective step
    /// that reads those facts. Replay restores exactly the recorded ranks and
    /// never a later one. `through_rank` is the prefix bound the opener chose
    /// at record time; the outcome lists what was actually incorporated.
    IncorporateGroupSettlements {
        group_key: String,
        through_rank: u64,
    },
    /// The recorded presentation boundary (ADR 0099 §6, FIG-3420): folds the
    /// session's ordered presentation steps over this settled output once and
    /// journals the resulting [`ToolPresentation`](super::ToolPresentation).
    /// Replay serves the recorded outcome and never re-runs a step.
    ///
    /// The command names only recorded facts. How long the call took is an
    /// observation: a redrive serves a journaled attempt at once and re-runs an
    /// orchestrating body against its recorded effects, so a duration here
    /// would move the envelope of a healthy redrive. The steps still read the
    /// live duration from the local executor, and the outcome they fold to is
    /// what replay serves.
    PresentToolResult {
        call_id: String,
        tool_name: String,
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
        language: String,
        code: String,
    },
    /// Write the Pending Turn Input row that admits a turn (ADR 0069 §1),
    /// through the effect journal so a replaying engine re-derives the
    /// admission rather than re-performing it (ADR 0069 §6).
    AcceptTurnInput {
        draft: Box<crate::PendingTurnInputDraft>,
    },
    /// Claim the initial drive set of the input `AcceptTurnInput` just admitted
    /// (ADR 0069 §6). The outcome journals the claimed rows with their content
    /// and claim token, exactly as a checkpoint journals its claim set, so a
    /// replaying turn drives the same rows under the same authority and never
    /// reads pending rows. The envelope names only the accepted input: the
    /// lease fence and owner are captured by the local executor, so the
    /// envelope hashes the same under every lease generation.
    ClaimAcceptedTurnInput {
        input_id: crate::InputId,
    },
    Checkpoint {
        checkpoint: CheckpointKind,
    },
    /// Build and journal the environment the next protocol iteration's model
    /// call runs under (FIG-3538); every sync carries it, the protocol-start
    /// one included (FIG-3587).
    SyncExecutionEnvironment,
    /// Sleep for a relative duration or until an absolute wall-clock deadline.
    ///
    /// The intent is the journaled parameter, never a duration derived from the
    /// clock at execution time, so a deadline-bearing sleep replays against the
    /// same envelope (FIG-2968).
    Sleep {
        spec: SleepSpec,
    },
    AwaitEvent {
        key: crate::AwaitEventKey,
    },
    PeekAwaitEvent {
        key: crate::AwaitEventKey,
    },
    LanguageRuntimeValue {
        operation: String,
    },
}

// Measured 200 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<RuntimeEffectCommand>() <= 256);

impl RuntimeEffectCommand {
    /// The completion key a group child running this command parks on, when
    /// it is a deferrable tool child (see
    /// [`ToolChildRequest::completion_wait`](super::ToolChildRequest::completion_wait)).
    /// Every other command delivers no completion to a key of its own.
    #[must_use]
    pub fn group_child_completion_wait(
        &self,
    ) -> Option<(crate::ExecutionScope, crate::AwaitEventWaitIdentity)> {
        match self {
            Self::ToolInvocation { request } => request.completion_wait(),
            _ => None,
        }
    }

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
    /// snapshots. A recorded `ExecResponse` cannot rebuild that state, so
    /// replay re-runs the cell deterministically, and the cell's own LLM, tool
    /// and durable effects answer from their journal rows. Restate maps the
    /// command to a direct local call; the journal-row hosts skip the claim.
    pub fn replays_by_reexecution(&self) -> bool {
        matches!(self, Self::ExecCode { .. })
    }

    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::AssistantResponseHooks { .. } => RuntimeEffectKind::AssistantResponseHooks,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,
            Self::ToolInvocation { .. } => RuntimeEffectKind::ToolInvocation,
            Self::IncorporateGroupSettlements { .. } => {
                RuntimeEffectKind::IncorporateGroupSettlements
            }
            Self::PresentToolResult { .. } => RuntimeEffectKind::PresentToolResult,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::AcceptTurnInput { .. } => RuntimeEffectKind::AcceptTurnInput,
            Self::ClaimAcceptedTurnInput { .. } => RuntimeEffectKind::ClaimAcceptedTurnInput,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::Sleep { .. } => RuntimeEffectKind::Sleep,
            Self::AwaitEvent { .. } => RuntimeEffectKind::AwaitEvent,
            Self::PeekAwaitEvent { .. } => RuntimeEffectKind::PeekAwaitEvent,
            Self::LanguageRuntimeValue { .. } => RuntimeEffectKind::LanguageRuntimeValue,
        }
    }
}

/// Serializable operation against the process admin plane.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// justification: RuntimeEffectCommand already boxes every ProcessCommand, so boxing its Start payload again is redundant.
#[allow(clippy::large_enum_variant)]
pub enum ProcessCommand {
    Start {
        registration: ProcessRegistration,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        observers: Vec<SessionId>,
        /// Captured environment carried inside the journal admission and
        /// persisted by the local executor before process registration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env_spec: Option<crate::ProcessExecutionEnvSpec>,
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
        process_ids: Vec<ProcessId>,
    },
    DeleteSession {
        session_id: SessionId,
    },
    Await {
        process_ref: crate::ProcessRef,
    },
    /// Arm the process terminal as the resolver of one durable wait, without
    /// waiting for it here.
    ///
    /// This is the command half of
    /// [`PendingResolver::ProcessTerminal`](crate::PendingResolver::ProcessTerminal).
    /// It returns as soon as the boundary has taken responsibility for the
    /// resolution, so the turn that issued it goes on to park on `key` through
    /// the ordinary [`RuntimeEffectCommand::AwaitEvent`] path. Arming is
    /// idempotent: the same `(process_ref, key)` may be armed on every redrive
    /// of the parked turn, and the first terminal to land resolves the wait
    /// exactly once.
    AttachTerminal {
        process_ref: crate::ProcessRef,
        key: crate::AwaitEventKey,
    },
    Cancel {
        process_ref: crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribution: Option<crate::RuntimeReplayAttribution>,
    },
    CancelRefused {
        process_id: ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        refusal: crate::PluginError,
    },
    Signal {
        process_ref: crate::ProcessRef,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    },
    EmitEvent {
        process_id: ProcessId,
        request: crate::ProcessEventAppendRequest,
    },
    /// The journaled CAS write a `RegisterProcessDefinition` intent realizes
    /// through (FIG-3470): the intent resolves the pinned reference and its
    /// compare-and-swap expectation first, then the durable write crosses the
    /// runtime-effect seam like every other journaled admission, so a redrive
    /// replays the same registration instead of issuing a second write.
    RegisterDefinition {
        owner_scope: crate::TriggerOwnerScope,
        name: String,
        /// The engine-resolved definition reference the row pins.
        pinned: crate::ProcessDefinitionRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expectation: Option<crate::ProcessDefinitionExpectation>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ProcessCommandDecode {
    Start {
        registration: ProcessRegistration,
        #[serde(default)]
        observers: Vec<SessionId>,
        #[serde(default)]
        env_spec: Box<Option<crate::ProcessExecutionEnvSpec>>,
        #[serde(default)]
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
        process_ids: Vec<ProcessId>,
    },
    DeleteSession {
        session_id: SessionId,
    },
    Await {
        process_ref: crate::ProcessRef,
    },
    AttachTerminal {
        process_ref: crate::ProcessRef,
        key: crate::AwaitEventKey,
    },
    Cancel {
        process_ref: crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        #[serde(default)]
        attribution: Option<crate::RuntimeReplayAttribution>,
    },
    CancelRefused {
        process_id: ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        refusal: crate::PluginError,
    },
    Signal {
        process_ref: crate::ProcessRef,
        signal_name: String,
        signal_id: String,
        request: crate::ProcessEventAppendRequest,
    },
    EmitEvent {
        process_id: ProcessId,
        request: crate::ProcessEventAppendRequest,
    },
    RegisterDefinition {
        owner_scope: crate::TriggerOwnerScope,
        name: String,
        pinned: crate::ProcessDefinitionRef,
        #[serde(default)]
        expectation: Option<crate::ProcessDefinitionExpectation>,
    },
}

impl<'de> Deserialize<'de> for ProcessCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(object) = value.as_object()
            && matches!(
                object.get("op").and_then(serde_json::Value::as_str),
                Some("await" | "cancel" | "signal")
            )
            && object.contains_key("process_id")
            && !object.contains_key("process_ref")
        {
            return Err(serde::de::Error::custom(
                "process_reference_format_cutover: a pre-incarnation process command cannot be replayed because its bare process_id does not identify one process lifetime",
            ));
        }
        let decoded: ProcessCommandDecode =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(match decoded {
            ProcessCommandDecode::Start {
                registration,
                observers,
                env_spec,
                execution_context,
            } => Self::Start {
                registration,
                observers,
                env_spec: *env_spec,
                execution_context,
            },
            ProcessCommandDecode::List {
                session_scope,
                mode,
            } => Self::List {
                session_scope,
                mode,
            },
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
            ProcessCommandDecode::Await { process_ref } => Self::Await { process_ref },
            ProcessCommandDecode::AttachTerminal { process_ref, key } => {
                Self::AttachTerminal { process_ref, key }
            }
            ProcessCommandDecode::Cancel {
                process_ref,
                origin,
                requester,
                attribution,
            } => Self::Cancel {
                process_ref,
                origin,
                requester,
                attribution,
            },
            ProcessCommandDecode::CancelRefused {
                process_id,
                origin,
                requester,
                refusal,
            } => Self::CancelRefused {
                process_id,
                origin,
                requester,
                refusal,
            },
            ProcessCommandDecode::Signal {
                process_ref,
                signal_name,
                signal_id,
                request,
            } => Self::Signal {
                process_ref,
                signal_name,
                signal_id,
                request,
            },
            ProcessCommandDecode::EmitEvent {
                process_id,
                request,
            } => Self::EmitEvent {
                process_id,
                request,
            },
            ProcessCommandDecode::RegisterDefinition {
                owner_scope,
                name,
                pinned,
                expectation,
            } => Self::RegisterDefinition {
                owner_scope,
                name,
                pinned,
                expectation,
            },
        })
    }
}

fn boxed_process_execution_context_is_empty(context: &ProcessExecutionContext) -> bool {
    context.is_empty()
}

type CheckpointOutcome = Result<CheckpointDelivery, RuntimeEffectControllerError>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointClaimSet {
    // Checkpoint replay skips the local executor that acquired these claims.
    // Journal them with the delivery so the replaying turn carries the same
    // settlement authority into its atomic final commit. Outcomes written by
    // older binaries have no claim set: one queued-work row and one active
    // turn-input row per in-flight turn can be redelivered by the next lease
    // generation, without loss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_work_claims: Vec<crate::QueuedWorkClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_claim: Option<crate::TurnInputClaim>,
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
            // Effect IDs are persisted replay identity and retain their
            // pre-incarnation spelling. The journaled command payload carries
            // the structural ProcessRef and refuses a superseded lifetime.
            Self::Await { process_ref } => format!("process:await:{}", process_ref.process_id),
            // One arming per (process, wait): a turn may park several
            // waits on the same process, and each redrive re-issues the
            // same id so the arming replays against its own journal entry
            // instead of colliding with the terminal wait above.
            Self::AttachTerminal { process_ref, key } => format!(
                "process:attach-terminal:{}:{}",
                process_ref.process_id, key.key_id
            ),
            Self::Cancel { process_ref, .. } => {
                format!("process:cancel:{}", process_ref.process_id)
            }
            Self::CancelRefused { process_id, .. } => {
                format!("process:cancel:{process_id}")
            }
            Self::Signal {
                process_ref,
                signal_name,
                signal_id,
                ..
            } => {
                format!(
                    "process:signal:{}:signal.{signal_name}:{signal_id}",
                    process_ref.process_id
                )
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
            Self::RegisterDefinition {
                owner_scope, name, ..
            } => format!(
                "process:register-definition:{}:{name}",
                owner_scope.namespace()
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
    /// The boundary has taken responsibility for resolving the armed wait.
    ///
    /// Deliberately payload-free: the arming says nothing about the process's
    /// state, and the resolution itself arrives through the await-event seam,
    /// not through this outcome.
    AttachTerminal,
    Cancel {
        record: Box<ProcessRecord>,
    },
    CancelRefused {
        refusal: crate::PluginError,
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
    RegisterDefinition {
        registration: Box<crate::ProcessDefinitionRegistration>,
    },
}

// Measured 88 B on rustc 1.97.0, x86_64-unknown-linux-gnu (FIG-595).
const _: () = assert!(std::mem::size_of::<ProcessEffectOutcome>() <= 112);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolAttemptEffectOutcome {
    pub launch: ToolAttemptLaunch,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<ToolTriggerEffectOutcome>,
    /// The attempt-local `EnqueueMessages` facts and managed LLM usage this
    /// attempt produced, journaled with it and restored into the dispatch
    /// buffers by whoever consumes this outcome — identically whether it was
    /// just executed or served by replay (ADR 0099 §6, §13).
    #[serde(default)]
    pub capture: ToolAttemptCapture,
}

/// What one tool child of a durable effect group settled on, unpacked.
///
/// The read side of
/// [`RuntimeEffectOutcome::ToolInvocation`](RuntimeEffectOutcome::ToolInvocation).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInvocationEffectOutcome {
    /// The terminal the per-leaf coordinator produced for this child.
    pub outcome: crate::tool_dispatch::ToolDispatchOutcome,
    /// The child's complete semantic record: realized intent outcomes, realized
    /// started-process identities, trigger receipts, committed checkpoint
    /// messages, per-attempt usage deltas and the resolved `ModelToolReturn`
    /// (ADR 0099 §6, §13).
    pub settlement: ToolSettlement,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolAttemptLaunch {
    Done {
        record: Box<crate::ToolCallRecord>,
        intents: crate::ToolIntents,
    },
    Pending {
        // Boxed: the canonical `ExecutionScope` inside the key dominates this
        // enum's size.
        key: Box<crate::AwaitEventKey>,
        pending: crate::PendingCompletion,
        duration_ms: u64,
    },
}

/// What phase 1 of a turn's LLM call recorded, decoded for the driver.
#[derive(Debug)]
pub struct RuntimeLlmCallOutcome {
    pub result: Result<LlmResponse, LlmCallError>,
    pub text_streamed: bool,
    pub call_record: Option<crate::LlmCallRecord>,
    pub stream: LlmStreamRecord,
}

/// What a turn's provider stream left behind that later steps read: recorded
/// with phase 1's outcome so a replay reads it from the journal, never from
/// the memory of the worker that streamed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmStreamRecord {
    /// The reasoning blocks the live stream already published, so the driver
    /// publishes the completed response's remaining reasoning the same way on
    /// every replay.
    pub reasoning_published: Vec<crate::llm::types::StreamBlockIdentity>,
    /// Each plugin's stream-hook end state, which phase 2's
    /// [`RuntimeEffectCommand::AssistantResponseHooks`] carries.
    pub stream_hook_states: Vec<AssistantStreamHookState>,
}

/// The state one plugin's stream hooks reached when the provider stream
/// finished (see [`crate::plugin::AssistantStreamFinishedHook`]).
///
/// Recorded with phase 1's outcome and handed to the same plugin's
/// assistant-response hook in phase 2, so the derivation never depends on
/// which worker streamed the completion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantStreamHookState {
    pub plugin_id: String,
    pub state: serde_json::Value,
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
        /// The attempt-local facts the attempt produced: `EnqueueMessages`
        /// directives and managed LLM usage. Journaled with the attempt so a
        /// replay restores them rather than re-running their producers — a
        /// crash after the attempt committed but before its invocation settled
        /// would otherwise drop them (ADR 0099 §13). Absent when the attempt
        /// captured nothing, so attempts that produced no facts serialize
        /// exactly as they did before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capture: Option<Box<ToolAttemptCapture>>,
    },
    /// What one tool child of a durable effect group settled on
    /// (ADR 0099 §2, §6, §13).
    ///
    /// The counterpart of
    /// [`ToolInvocation`](RuntimeEffectCommand::ToolInvocation), and not a
    /// [`ToolAttempt`](Self::ToolAttempt): that is one attempt's atomic body,
    /// so it cannot express a child that retried.
    ///
    /// `outcome` is exactly the terminal the per-leaf coordinator produced;
    /// `settlement` is the child's complete semantic record, including the
    /// `ModelToolReturn` the singleton plugin projector resolved at the
    /// child's own presentation boundary. The opener incorporates the
    /// settlement as recorded evidence; it never re-executes a declaration and
    /// never re-runs the projector.
    ///
    /// There is deliberately no pending arm. Deferred completion is
    /// *coordination* and runs at handler level inside the driver (§2), so a
    /// child that parked has already been awaited by the time this outcome
    /// exists: a group child settles once, and a journaled "still pending" is a
    /// state no reader of a settlement could act on.
    ToolInvocation {
        outcome: Box<crate::tool_dispatch::ToolDispatchOutcome>,
        /// The §6/§13 settlement the child accumulated in its own address
        /// space. Always journaled: a child that reached a terminal always
        /// produced a settled presentation.
        settlement: Box<ToolSettlement>,
    },
    /// The group-settlement prefix an
    /// [`IncorporateGroupSettlements`](RuntimeEffectCommand::IncorporateGroupSettlements)
    /// command incorporated: the recorded mapping replay re-applies, rank by
    /// rank, and nothing past it (ADR 0099 §6).
    IncorporateGroupSettlements {
        incorporated: Vec<super::group::IncorporatedGroupRank>,
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
    /// The accepted input's initial drive set, journaled so replay drives the
    /// same rows under the same settlement authority (ADR 0069 §6).
    ClaimAcceptedTurnInput {
        drive: crate::AcceptedTurnInputDrive,
    },
    Checkpoint {
        result: CheckpointOutcome,
        #[serde(default)]
        claims: Box<CheckpointClaimSet>,
    },
    SyncExecutionEnvironment {
        result: Result<Option<ExecutionEnvironmentSync>, String>,
        /// The replay-key grammar the code executor journals this
        /// iteration's cells' nested effects under (FIG-3586). A cell
        /// re-executed on replay runs only under the grammar its iteration's
        /// journaled sync names; a record without it — `None`, and every
        /// record written before the field existed — was written by a build
        /// whose cells keyed their effects under a retired grammar, so its
        /// cells are refused rather than re-issued live.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_replay_grammar: Option<u32>,
    },
    Sleep,
    AwaitEvent {
        resolution: crate::Resolution,
    },
    PeekAwaitEvent {
        resolution: Option<crate::Resolution>,
    },
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
    pub model: String,
    pub messages: Vec<LlmMessage>,
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
        attachment_store: &crate::SessionAttachmentStore,
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
            model_variant: request.model_variant.clone(),
            model_capability: request.model_capability.clone(),
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

async fn durable_attachment_source(
    attachment: &AttachmentSource,
    attachment_store: &crate::SessionAttachmentStore,
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

/// A journaled execution-environment sync as served: its result, and the cell
/// replay-key grammar its record names (FIG-3586).
pub type ServedExecutionEnvironmentSync = (
    Result<Option<ExecutionEnvironmentSync>, String>,
    Option<u32>,
);

impl RuntimeEffectOutcome {
    pub fn into_llm_call(self) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError> {
        match self {
            Self::LlmCall {
                result,
                text_streamed,
                call_record,
                stream,
            } => Ok(RuntimeLlmCallOutcome {
                result: *result,
                text_streamed,
                call_record,
                stream: *stream,
            }),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LlmCall,
                other.kind(),
            )),
        }
    }

    pub fn into_assistant_response_hooks(
        self,
    ) -> Result<RuntimeAssistantResponseHooksOutcome, RuntimeEffectControllerError> {
        match self {
            Self::AssistantResponseHooks { response, events } => Ok((*response, events)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AssistantResponseHooks,
                other.kind(),
            )),
        }
    }

    pub fn into_direct_response(
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
            Self::ToolAttempt {
                launch,
                triggers,
                capture,
            } => {
                let capture = capture.map(|capture| *capture).unwrap_or_default();
                capture.validate()?;
                Ok(ToolAttemptEffectOutcome {
                    launch: *launch,
                    triggers,
                    capture,
                })
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolAttempt,
                other.kind(),
            )),
        }
    }

    /// Unpacks a settled tool child of a durable effect group.
    ///
    /// Validates the settlement rather than trusting it: a journal entry
    /// written by a build whose settlement format this build cannot read
    /// completely is refused here, where the outcome is consumed, instead of
    /// being served to an opener as a prefix of what its child actually
    /// produced.
    pub fn into_tool_invocation_effect(
        self,
    ) -> Result<ToolInvocationEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::ToolInvocation {
                outcome,
                settlement,
            } => {
                settlement.validate()?;
                Ok(ToolInvocationEffectOutcome {
                    outcome: *outcome,
                    settlement: *settlement,
                })
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolInvocation,
                other.kind(),
            )),
        }
    }

    /// Unpacks the recorded incorporation prefix of a durable effect group.
    pub fn into_incorporate_group_settlements(
        self,
    ) -> Result<Vec<super::group::IncorporatedGroupRank>, RuntimeEffectControllerError> {
        match self {
            Self::IncorporateGroupSettlements { incorporated } => Ok(incorporated),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::IncorporateGroupSettlements,
                other.kind(),
            )),
        }
    }

    /// Unpacks the recorded presentation of one settled tool result.
    ///
    /// Validates the record rather than trusting it: a journal entry written
    /// by a build whose presentation format this build cannot read completely
    /// is refused here, where the outcome is consumed, instead of serving the
    /// model a prefix of what the chain produced.
    pub fn into_tool_presentation(
        self,
    ) -> Result<super::ToolPresentation, RuntimeEffectControllerError> {
        match self {
            Self::PresentToolResult { presentation } => {
                presentation.validate()?;
                Ok(*presentation)
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::PresentToolResult,
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

    pub fn into_accepted_turn_input(
        self,
    ) -> Result<crate::PendingTurnInput, RuntimeEffectControllerError> {
        match self {
            Self::AcceptTurnInput { accepted } => Ok(*accepted),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AcceptTurnInput,
                other.kind(),
            )),
        }
    }

    pub fn into_accepted_turn_input_drive(
        self,
    ) -> Result<crate::AcceptedTurnInputDrive, RuntimeEffectControllerError> {
        match self {
            Self::ClaimAcceptedTurnInput { drive } => Ok(drive),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ClaimAcceptedTurnInput,
                other.kind(),
            )),
        }
    }

    pub fn into_exec_code(
        self,
    ) -> Result<Result<ExecResponse, crate::ExecCodeFailure>, RuntimeEffectControllerError> {
        match self {
            Self::ExecCode { result } => Ok(*result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ExecCode,
                other.kind(),
            )),
        }
    }

    pub fn into_checkpoint(
        self,
    ) -> Result<(CheckpointOutcome, CheckpointClaimSet), RuntimeEffectControllerError> {
        match self {
            Self::Checkpoint { result, claims } => Ok((result, *claims)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Checkpoint,
                other.kind(),
            )),
        }
    }

    /// The sync's result and the cell replay-key grammar its record names.
    pub fn into_sync_execution_environment(
        self,
    ) -> Result<ServedExecutionEnvironmentSync, RuntimeEffectControllerError> {
        match self {
            Self::SyncExecutionEnvironment {
                result,
                cell_replay_grammar,
            } => Ok((result, cell_replay_grammar)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::SyncExecutionEnvironment,
                other.kind(),
            )),
        }
    }

    pub fn into_await_event(self) -> Result<crate::Resolution, RuntimeEffectControllerError> {
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

    /// Extracts a journaled language-runtime value.
    pub fn into_language_runtime_value(
        self,
    ) -> Result<serde_json::Value, RuntimeEffectControllerError> {
        match self {
            Self::LanguageRuntimeValue { value } => Ok(value),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LanguageRuntimeValue,
                other.kind(),
            )),
        }
    }

    /// Exposes kind to effect-host implementors while executing or replaying a runtime effect.
    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::AssistantResponseHooks { .. } => RuntimeEffectKind::AssistantResponseHooks,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,
            Self::ToolInvocation { .. } => RuntimeEffectKind::ToolInvocation,
            Self::IncorporateGroupSettlements { .. } => {
                RuntimeEffectKind::IncorporateGroupSettlements
            }
            Self::PresentToolResult { .. } => RuntimeEffectKind::PresentToolResult,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::AcceptTurnInput { .. } => RuntimeEffectKind::AcceptTurnInput,
            Self::ClaimAcceptedTurnInput { .. } => RuntimeEffectKind::ClaimAcceptedTurnInput,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::Sleep => RuntimeEffectKind::Sleep,
            Self::AwaitEvent { .. } => RuntimeEffectKind::AwaitEvent,
            Self::PeekAwaitEvent { .. } => RuntimeEffectKind::PeekAwaitEvent,
            Self::LanguageRuntimeValue { .. } => RuntimeEffectKind::LanguageRuntimeValue,
        }
    }
}

impl From<RuntimeEffectInvocation> for crate::RuntimeInvocation {
    fn from(invocation: RuntimeEffectInvocation) -> Self {
        invocation.into_runtime_invocation()
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    fn invocation(kind: RuntimeEffectKind) -> RuntimeEffectInvocation {
        let _ = kind;
        RuntimeEffectInvocation::new(
            EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
                .expect("valid rejection-test address"),
            RuntimeAttribution::for_session("session"),
            "effect",
        )
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn process_transfer_v1_identity_golden() {
        let process_ids = vec![
            ProcessId::from("process:a:b"),
            ProcessId::from("process\0b"),
            ProcessId::from("λ"),
        ];
        assert_eq!(
            hex(&process_transfer_set_preimage(&process_ids)),
            "6c6173682d737461626c652d6964656e74697479020100000000000000196c6173682e70726f636573732d7472616e736665722d7365740000000000000003000000000000000b70726f636573733a613a62000000000000000970726f6365737300620000000000000002cebb"
        );
        assert_eq!(
            process_transfer_set_identity(&process_ids),
            "process-transfer-set:v1:blake3:aedb74c73220c6ad3d081471ab61f1505070fa03dbb0013529d4cd84fd48cc0a"
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

    fn assert_rejected(
        invocation: RuntimeEffectInvocation,
        command: RuntimeEffectCommand,
        expected_code: &str,
    ) {
        let error = RuntimeEffectEnvelope::try_new(invocation, command)
            .expect_err("invalid envelope must be rejected");
        assert_eq!(error.code.as_str(), expected_code);
    }

    #[test]
    fn rejects_empty_effect_id() {
        let error = RuntimeEffectInvocation::try_new(
            EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
                .expect("valid rejection-test address"),
            RuntimeAttribution::for_session("session"),
            "  ",
        )
        .expect_err("empty descriptive labels are refused");
        assert_eq!(error.code.as_str(), "runtime_effect_invocation_subject");
    }

    #[test]
    fn rejects_empty_address_replay_key() {
        let mut empty_address = invocation(RuntimeEffectKind::Sleep);
        empty_address.address.replay_key.clear();
        assert_rejected(
            empty_address,
            RuntimeEffectCommand::Sleep {
                spec: crate::SleepSpec::For { duration_ms: 1 },
            },
            "runtime_effect_replay_required",
        );
    }

    #[test]
    fn effect_header_round_trips_without_universal_subject_or_replay_slots() {
        let invocation =
            invocation(RuntimeEffectKind::Sleep).with_caused_by(Some(CausalRef::Process {
                process_id: ProcessId::from("process"),
            }));
        let encoded = serde_json::to_value(&invocation).expect("effect header encodes");
        assert!(encoded.get("address").is_some());
        assert!(encoded.get("subject").is_none());
        assert!(encoded.get("replay").is_none());
        assert_eq!(
            serde_json::from_value::<RuntimeEffectInvocation>(encoded)
                .expect("effect header decodes"),
            invocation
        );
    }

    #[test]
    fn legacy_universal_effect_header_is_refused() {
        let legacy = serde_json::to_value(RuntimeInvocation::effect(
            EffectAddress::new(ExecutionScope::runtime_operation("session"), "replay")
                .expect("valid legacy address"),
            RuntimeAttribution::for_session("session"),
            "effect",
        ))
        .expect("legacy header encodes");
        assert!(serde_json::from_value::<RuntimeEffectInvocation>(legacy).is_err());
    }

    #[test]
    fn session_node_identity_is_structural_and_missing_identity_is_refused() {
        let invocation = RuntimeInvocation {
            attribution: RuntimeAttribution::none(),
            subject: RuntimeSubject::SessionNode {
                session_id: SessionId::from("session"),
                node_id: "node".to_string(),
            },
            caused_by: None,
            replay: None,
        };
        assert_eq!(
            serde_json::from_value::<RuntimeInvocation>(
                serde_json::to_value(&invocation).expect("session-node invocation encodes")
            )
            .expect("session-node invocation decodes")
            .causal_ref(),
            invocation.causal_ref()
        );
        let missing = serde_json::json!({
            "attribution": {},
            "subject": {"type": "session_node", "node_id": "node"}
        });
        assert!(serde_json::from_value::<RuntimeInvocation>(missing).is_err());
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
}

#[cfg(test)]
mod cell_replay_grammar_tests {
    use super::*;

    /// T11 (FIG-3586): a sync outcome journaled before the grammar stamp
    /// existed serves no grammar, which a code executor refuses; a stamped one
    /// round-trips.
    #[test]
    fn a_sync_journaled_before_the_stamp_serves_no_grammar() {
        let stamped = RuntimeEffectOutcome::SyncExecutionEnvironment {
            result: Ok(None),
            cell_replay_grammar: Some(2),
        };
        let mut wire = serde_json::to_value(&stamped).expect("encode the stamped sync");
        let decoded: RuntimeEffectOutcome =
            serde_json::from_value(wire.clone()).expect("decode the stamped sync");
        assert_eq!(
            decoded
                .into_sync_execution_environment()
                .expect("a sync outcome")
                .1,
            Some(2)
        );

        let shown = wire.to_string();
        let outcome = wire
            .as_object_mut()
            .unwrap_or_else(|| panic!("a sync outcome is an object: {shown}"));
        let removed = outcome.remove("cell_replay_grammar").is_some()
            || outcome
                .values_mut()
                .filter_map(serde_json::Value::as_object_mut)
                .any(|fields| fields.remove("cell_replay_grammar").is_some());
        assert!(removed, "the stamp is on the wire: {shown}");
        let legacy: RuntimeEffectOutcome =
            serde_json::from_value(wire).expect("a pre-stamp sync still decodes");
        assert_eq!(
            legacy
                .into_sync_execution_environment()
                .expect("a sync outcome")
                .1,
            None,
            "a pre-stamp sync names no grammar"
        );
    }
}
