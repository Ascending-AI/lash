use crate::CausalRef;
use serde::{Deserialize, Serialize};

use crate::{EffectAddress, ProcessId, SessionId, TurnId};

use crate::runtime_error::RuntimeEffectControllerError;

pub fn process_transfer_set_preimage(process_ids: &[ProcessId]) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-transfer-set",
        PROCESS_TRANSFER_FAMILY_VERSION,
    );
    identity.sequence(process_ids, |identity, process_id| {
        identity.string(process_id);
    });
    identity.finish()
}

pub fn process_transfer_set_identity(process_ids: &[ProcessId]) -> String {
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
    /// Phase 2 of the staged LLM-call boundary: host response hooks derive a
    /// transformed response from the raw completion phase 1 already journaled.
    AssistantResponseHooks,
    Direct,
    ToolAttempt,
    /// One tool child of a durable effect group, driven at invocation level
    /// (ADR 0099 §2). Distinct from [`ToolAttempt`](Self::ToolAttempt), which is
    /// the atomic body of one attempt.
    ToolInvocation,
    /// The opener-journaled record of a group's incorporated settlement prefix
    /// (ADR 0099 §6): which ranks the opener applied before an externally
    /// effective step, so replay restores exactly that mapping.
    IncorporateGroupSettlements,
    /// The recorded presentation boundary of one settled tool result
    /// (ADR 0099 §6, FIG-3420): the ordered presentation steps folded once,
    /// journaled so replay serves the recorded return and retained artifacts
    /// without re-running a step.
    PresentToolResult,
    ToolParentEnd,
    Trigger,
    Process,
    ExecCode,
    /// Durable admission of a turn input (ADR 0069 section 6).
    AcceptTurnInput,
    /// The journaled initial drive set of an accepted turn input: the rows the
    /// turn claimed right after acceptance, with their settlement authority
    /// (ADR 0069 section 6). Replay returns it and never re-reads pending rows.
    ClaimAcceptedTurnInput,
    Checkpoint,
    SyncExecutionEnvironment,
    /// The recorded read of a tool child's execution environment (FIG-3683):
    /// the store is read once and every replay serves the recorded spec.
    LoadExecutionEnv,
    Sleep,
    AwaitEvent,
    PeekAwaitEvent,
    LanguageRuntimeValue,
}

impl RuntimeEffectKind {
    /// The stable snake-case kind label persisted in replay diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlmCall => "llm_call",
            Self::AssistantResponseHooks => "assistant_response_hooks",
            Self::Direct => "direct",
            Self::ToolAttempt => "tool_attempt",
            Self::ToolInvocation => "tool_invocation",
            Self::IncorporateGroupSettlements => "incorporate_group_settlements",
            Self::PresentToolResult => "present_tool_result",
            Self::ToolParentEnd => "tool_parent_end",
            Self::Trigger => "trigger",
            Self::Process => "process",
            Self::ExecCode => "exec_code",
            Self::AcceptTurnInput => "accept_turn_input",
            Self::ClaimAcceptedTurnInput => "claim_accepted_turn_input",
            Self::Checkpoint => "checkpoint",
            Self::SyncExecutionEnvironment => "sync_execution_environment",
            Self::LoadExecutionEnv => "load_execution_env",
            Self::Sleep => "sleep",
            Self::AwaitEvent => "await_event",
            Self::PeekAwaitEvent => "peek_await_event",
            Self::LanguageRuntimeValue => "language_runtime_value",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuntimeAttribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl RuntimeAttribution {
    pub fn is_none(&self) -> bool {
        self.session_id.is_none()
            && self.turn_id.is_none()
            && self.turn_index.is_none()
            && self.protocol_iteration.is_none()
    }

    pub fn none() -> Self {
        Self {
            session_id: None,
            turn_id: None,
            turn_index: None,
            protocol_iteration: None,
        }
    }

    pub fn for_session(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::none()
        }
    }

    pub fn for_turn(
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
        turn_index: usize,
        protocol_iteration: usize,
    ) -> Self {
        Self {
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            turn_index: Some(turn_index),
            protocol_iteration: Some(protocol_iteration),
        }
    }

    /// A turn's attribution before its admission fixed a turn index: the
    /// acceptance of its input and the drive that admits it (FIG-3682). A
    /// turn index read here would come from the live head, which a replay
    /// after the turn's own commit no longer shares.
    pub fn for_turn_admission(
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
    ) -> Self {
        Self {
            session_id: Some(session_id.into()),
            turn_id: Some(turn_id.into()),
            ..Self::none()
        }
    }

    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id.trim().is_empty())
            || self
                .turn_id
                .as_ref()
                .is_some_and(|turn_id| turn_id.trim().is_empty())
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
                "runtime attribution identifiers must be non-empty when present",
            ));
        }
        if self.turn_id.is_some() && self.session_id.is_none() {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
                "runtime turn attribution requires real session attribution",
            ));
        }
        if (self.turn_index.is_some() || self.protocol_iteration.is_some())
            && self.turn_id.is_none()
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectInvocationSubject,
                "runtime progress attribution requires real turn attribution",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuntimeReplay {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<RuntimeReplayAttribution>,
}

/// Structural attribution for a durable replay entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum RuntimeReplayAttribution {
    ToolIntent(crate::ToolIntentIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeSubject {
    Effect {
        address: EffectAddress,
        effect_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_attribution: Option<RuntimeReplayAttribution>,
    },
    Process {
        process_id: ProcessId,
    },
    ProcessEvent {
        process_id: ProcessId,
        sequence: u64,
        event_type: String,
    },
    TriggerOccurrence {
        occurrence_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_incarnation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subscription_revision: Option<u64>,
    },
    SessionNode {
        session_id: SessionId,
        node_id: String,
    },
}

pub(crate) const PROCESS_TRANSFER_FAMILY_VERSION: u8 = 1;
/// Canonical lineage for a runtime-side invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuntimeInvocation {
    pub attribution: RuntimeAttribution,
    pub subject: RuntimeSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<RuntimeReplay>,
}
impl RuntimeInvocation {
    pub fn effect(
        address: EffectAddress,
        attribution: RuntimeAttribution,
        effect_id: impl Into<String>,
    ) -> Self {
        Self {
            attribution,
            subject: RuntimeSubject::Effect {
                address,
                effect_id: effect_id.into(),
                replay_attribution: None,
            },
            caused_by: None,
            replay: None,
        }
    }

    pub fn with_caused_by(mut self, caused_by: Option<CausalRef>) -> Self {
        self.caused_by = caused_by;
        self
    }

    pub fn effect_id(&self) -> Option<&str> {
        match &self.subject {
            RuntimeSubject::Effect { effect_id, .. } => Some(effect_id),
            _ => None,
        }
    }

    pub fn effect_address(&self) -> Option<&EffectAddress> {
        match &self.subject {
            RuntimeSubject::Effect { address, .. } => Some(address),
            _ => None,
        }
    }

    /// Exposes replay key to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn. Returns `None` when no replay key is present.
    pub fn replay_key(&self) -> Option<&str> {
        self.effect_address()
            .map(|address| address.replay_key.as_str())
            .or_else(|| self.replay.as_ref().map(|replay| replay.key.as_str()))
    }

    pub fn replay_attribution(&self) -> Option<&RuntimeReplayAttribution> {
        match &self.subject {
            RuntimeSubject::Effect {
                replay_attribution, ..
            } => replay_attribution.as_ref(),
            _ => self
                .replay
                .as_ref()
                .and_then(|replay| replay.attribution.as_ref()),
        }
    }

    #[must_use]
    pub fn with_replay_attribution(mut self, attribution: RuntimeReplayAttribution) -> Self {
        match &mut self.subject {
            RuntimeSubject::Effect {
                replay_attribution, ..
            } => *replay_attribution = Some(attribution),
            _ => panic!("runtime replay attribution can only be attached to an effect subject"),
        }
        self
    }

    /// Projects stable causal identity for protocol and effect-host implementors; each invocation
    /// subject maps to its corresponding causal-reference variant.
    pub fn causal_ref(&self) -> Option<CausalRef> {
        match &self.subject {
            RuntimeSubject::Effect { address, .. } => Some(CausalRef::Effect {
                address: address.clone(),
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
            RuntimeSubject::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            } => Some(CausalRef::TriggerOccurrence {
                occurrence_id: occurrence_id.clone(),
                subscription_id: subscription_id.clone(),
                subscription_incarnation: subscription_incarnation.clone(),
                subscription_revision: *subscription_revision,
            }),
            RuntimeSubject::SessionNode {
                session_id,
                node_id,
            } => Some(CausalRef::SessionNode {
                session_id: session_id.clone(),
                node_id: node_id.clone(),
            }),
        }
    }
}
