use serde::{Deserialize, Serialize};

use crate::{EffectAddress, ProcessId, SessionId, TurnId};

use super::RuntimeEffectControllerError;

const PROCESS_TRANSFER_FAMILY_VERSION: u8 = 1;

pub(super) fn process_transfer_set_preimage(process_ids: &[ProcessId]) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-transfer-set",
        PROCESS_TRANSFER_FAMILY_VERSION,
    );
    identity.sequence(process_ids, |identity, process_id| {
        identity.string(process_id);
    });
    identity.finish()
}

pub(super) fn process_transfer_set_identity(process_ids: &[ProcessId]) -> String {
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
    ToolBatch,
    ToolParentEnd,
    Trigger,
    Process,
    ExecCode,
    /// Durable admission of a turn input (ADR 0069 section 6).
    AcceptTurnInput,
    Checkpoint,
    SyncExecutionEnvironment,
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
            Self::ToolBatch => "tool_batch",
            Self::ToolParentEnd => "tool_parent_end",
            Self::Trigger => "trigger",
            Self::Process => "process",
            Self::ExecCode => "exec_code",
            Self::AcceptTurnInput => "accept_turn_input",
            Self::Checkpoint => "checkpoint",
            Self::SyncExecutionEnvironment => "sync_execution_environment",
            Self::Sleep => "sleep",
            Self::AwaitEvent => "await_event",
            Self::PeekAwaitEvent => "peek_await_event",
            Self::LanguageRuntimeValue => "language_runtime_value",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReplay {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<RuntimeReplayAttribution>,
}

/// Structural attribution for a durable replay entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum RuntimeReplayAttribution {
    ToolIntent(crate::ToolIntentIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
