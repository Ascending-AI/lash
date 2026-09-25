//! Process lifecycle envelopes: start/cancel/signal/await/list requests and
//! results, process records and summaries, event semantics, execution
//! environments, and runtime invocation provenance.

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::prompt::RemotePromptLayer;
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::tools::RemoteToolOutputContract;
use crate::turn_input::RemoteTurnInput;
use crate::turn_result::RemoteCausalRef;

mod lifecycle;
pub use lifecycle::{
    RemoteEffectOpener, RemoteOnParentEnd, RemoteParentScope, RemoteProcessLifecyclePolicy,
};

mod outcomes;
pub use outcomes::{
    RemoteObservedProcessFailure, RemoteProcessAwaitOutput, RemoteProcessToolCallOutcome,
    RemoteProcessToolCallOutput, RemoteProcessToolCancellation, RemoteProcessToolFailure,
    RemoteProcessToolFailureSource, RemoteProcessToolRetryStatus, RemoteToolFailureClass,
};

mod operations;
pub use operations::*;

#[cfg(test)]
mod frame_scope_tests;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionScope {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_id: Option<String>,
}

impl RemoteSessionScope {
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_frame_id: None,
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "session_id", &self.session_id)?;
        if let Some(agent_frame_id) = &self.agent_frame_id {
            require_non_empty(type_name, "agent_frame_id", agent_frame_id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct RemoteProcessExecutionEnvRef(String);

impl RemoteProcessExecutionEnvRef {
    pub const PREFIX: &'static str = "process-env:v6:blake3:";

    pub fn parse(value: impl Into<String>) -> Result<Self, RemoteProtocolError> {
        let value = value.into();
        if is_canonical_process_execution_env_ref(&value) {
            Ok(Self(value))
        } else {
            Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessExecutionEnvRef",
                message: "env_ref must match `process-env:v6:blake3:<64 lowercase hex>`"
                    .to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if is_canonical_process_execution_env_ref(&self.0) {
            Ok(())
        } else {
            Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "env_ref must match `process-env:v6:blake3:<64 lowercase hex>`"
                    .to_string(),
            })
        }
    }
}

impl std::fmt::Display for RemoteProcessExecutionEnvRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for RemoteProcessExecutionEnvRef {
    type Err = RemoteProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> serde::Deserialize<'de> for RemoteProcessExecutionEnvRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Refuses process ids the durable process key encoding cannot store.
///
/// The rule is core's `invalid_process_key_reason`, restated because the base
/// remote DTOs deliberately do not depend on `lash-core` (the `core-conversions`
/// feature is optional). The decoder re-runs core's own validator, so this is
/// an early, field-named refusal and not the authority (FIG-2985).
fn require_storable_process_key(
    type_name: &'static str,
    value: &str,
) -> Result<(), RemoteProtocolError> {
    let reason = if value.contains('\0') {
        "process_id must not contain NUL"
    } else if value.contains('#') {
        "process_id contains reserved segment separator `#`"
    } else {
        return Ok(());
    };
    Err(RemoteProtocolError::InvalidEnvelope {
        type_name,
        message: reason.to_string(),
    })
}

fn is_canonical_process_execution_env_ref(value: &str) -> bool {
    let Some(digest) = value.strip_prefix(RemoteProcessExecutionEnvRef::PREFIX) else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessOriginator {
    Host {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Session {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_frame_id: Option<String>,
    },
}

impl RemoteProcessOriginator {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Host { .. } => Ok(()),
            Self::Session {
                session_id,
                agent_frame_id,
            } => {
                require_non_empty(type_name, "session_id", session_id)?;
                if let Some(agent_frame_id) = agent_frame_id {
                    require_non_empty(type_name, "agent_frame_id", agent_frame_id)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessProvenance {
    pub originator: RemoteProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<RemoteCausalRef>,
}

impl RemoteProcessProvenance {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        self.originator.validate(type_name)?;
        if let Some(caused_by) = &self.caused_by {
            caused_by.validate(type_name)?;
        }
        Ok(())
    }
}
#[cfg(all(test, feature = "core-conversions"))]
mod core_process_status_label_tests;
/// The typed definition reference a durable process row pins: the engine that
/// owns the definition, the engine-owned definition value, and the signature
/// claimed for it. The claim is never authority (ADR 0095) — a peer's claim is
/// checked against the owning engine before any local row is created.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessDefinitionIdentity {
    pub engine_kind: String,
    #[serde(default)]
    pub value: serde_json::Value,
    pub signature: RemoteProcessSignature,
}

/// A signature claim travelling on a definition reference, or the explicit
/// absence of one (ADR 0090's unknown process type).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "signature", rename_all = "snake_case")]
pub enum RemoteProcessSignature {
    Unknown,
    Known { encoding: serde_json::Value },
}

impl RemoteProcessDefinitionIdentity {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "definition.engine_kind", &self.engine_kind)?;
        if self.value.is_null() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "definition value cannot be null".to_string(),
            });
        }
        Ok(())
    }
}

/// The kind and label a peer declares for a start whose input core owns
/// outright. A start request never carries a definition reference: only the
/// engine registry can put one on a durable row, and only after resolving it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteDeclaredProcessIdentity {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RemoteDeclaredProcessIdentity {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "identity.kind", &self.kind)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessIdentity {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<RemoteProcessDefinitionIdentity>,
}

impl RemoteProcessIdentity {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "identity.kind", &self.kind)?;
        if let Some(definition) = &self.definition {
            definition.validate(type_name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
// justification: this public remote DTO preserves its source-compatible inline SessionTurn construction and matching API.
#[allow(clippy::large_enum_variant)]
pub enum RemoteProcessInput {
    ToolCall {
        #[serde(default)]
        prepared_tool_call: serde_json::Value,
    },
    Engine {
        kind: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    SessionTurn {
        definition_key: String,
        #[serde(default)]
        create_request: serde_json::Value,
        turn_input: RemoteTurnInput,
        #[serde(default, skip_serializing_if = "RemoteToolOutputContract::is_static")]
        output_contract: RemoteToolOutputContract,
    },
    External {
        #[serde(default)]
        metadata: serde_json::Value,
    },
}

impl RemoteProcessInput {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::ToolCall { prepared_tool_call } => {
                // The payload stays opaque JSON on the wire, but core refuses a
                // registration whose prepared call has no call id or tool name
                // (FIG-2869), and the record decoder builds exactly that
                // registration. Refuse the same two fields here so peer input
                // fails with a named field rather than deeper in the decode
                // (FIG-2985).
                require_non_empty(
                    type_name,
                    "prepared_tool_call.call_id",
                    prepared_tool_call
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                )?;
                require_non_empty(
                    type_name,
                    "prepared_tool_call.tool_name",
                    prepared_tool_call
                        .get("tool_name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                )
            }
            Self::Engine { kind, payload: _ } => require_non_empty(type_name, "kind", kind),
            Self::SessionTurn {
                definition_key,
                create_request: _,
                turn_input,
                output_contract,
            } => {
                require_non_empty(type_name, "definition_key", definition_key)?;
                turn_input.validate()?;
                match output_contract {
                    RemoteToolOutputContract::Static => Ok(()),
                    RemoteToolOutputContract::FromInputSchema {
                        input_field,
                        default_schema: _,
                    } => require_non_empty(type_name, "output_contract.input_field", input_field),
                }
            }
            Self::External { metadata: _ } => Ok(()),
        }
    }

    /// Whether core requires a captured execution env for this input kind.
    ///
    /// Mirrors `validate_process_registration`: executable inputs carry an env
    /// ref and declarative ones must not (FIG-2985).
    fn requires_execution_env(&self) -> bool {
        match self {
            Self::ToolCall { .. } | Self::Engine { .. } => true,
            Self::SessionTurn { .. } | Self::External { .. } => false,
        }
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessStatus {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    /// Mirrors [`lash_core::ProcessStatus::CallerDeparted`]: durably
    /// distinguishable, deliberately never terminal.
    CallerDeparted,
}

impl RemoteProcessStatus {
    /// The wire label for this status.
    ///
    /// Nothing on the encode path reads it: the derived `status_label` field
    /// this used to validate is gone, and the label now lives only in
    /// `lash_core::ProcessStatus`. It is kept so the agreement test below can
    /// still prove the two vocabularies have not diverged.
    #[cfg(test)]
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
            Self::CallerDeparted => "caller_departed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessExternalRef {
    pub backend: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Execution segment the reference was minted for; absent reads as zero.
    /// Carried so a peer's compare-and-set sees the same ordinal the owning
    /// tier wrote, rather than silently flattening every segment to the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_ordinal: Option<u64>,
}

impl RemoteProcessExternalRef {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "external_ref.backend", &self.backend)?;
        require_non_empty(type_name, "external_ref.id", &self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWaitState {
    pub kind: RemoteProcessWaitKind,
    pub since_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteProcessWaitKind {
    Signal {
        name: String,
        event_type: String,
        key: String,
        ordinal: u64,
    },
}

/// Why a process parked: the serde form of the core park reason, arm for
/// arm (FIG-3659 NOW-B).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteParkReason {
    /// A code cell's re-execution issued a command its journal does not hold.
    ReplayDivergence { message: String },
    /// A code cell's journal was written under a replay-key grammar this
    /// build does not mint.
    KeyFormatCutover { message: String },
    /// A host tool binding the journal names is missing or changed.
    BindingDrift { message: String },
    /// A recorded effect's envelope no longer matches the one the redrive
    /// reconstructs.
    EffectReplayDivergence {
        effect_kind: String,
        message: String,
    },
    /// The session-state generation gate refused the redrive.
    SessionStateGenerationRefused {
        found: u32,
        current: u32,
        message: String,
    },
}

/// The park a process is in while its body refuses to replay its journal
/// (FIG-3659 NOW-B).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessPark {
    /// Why it parked, as its latest refusal said.
    pub reason: RemoteParkReason,
    /// The process event sequence of the fact that opened the park.
    pub park_id: u64,
    /// Epoch milliseconds of the park's first refusal.
    pub since_ms: u64,
    /// Epoch milliseconds of its latest refusal.
    pub last_refused_ms: u64,
    /// Refusals since the park opened.
    pub attempts: u32,
    /// Whether the latest run refused (a rerun under way clears it).
    pub refusing: bool,
}

impl RemoteProcessPark {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if self.attempts == 0 {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "a process park counts at least one refusal".to_string(),
            });
        }
        Ok(())
    }
}

impl RemoteProcessWaitState {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match &self.kind {
            RemoteProcessWaitKind::Signal {
                name,
                event_type,
                key,
                ordinal,
            } => {
                require_non_empty(type_name, "wait.name", name)?;
                require_non_empty(type_name, "wait.event_type", event_type)?;
                require_non_empty(type_name, "wait.key", key)?;
                if *ordinal == 0 {
                    return Err(RemoteProtocolError::InvalidEnvelope {
                        type_name,
                        message: "wait ordinal must be non-zero".to_string(),
                    });
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessRef {
    pub process_id: ProcessId,
    pub incarnation: u64,
}

impl RemoteProcessRef {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        if self.incarnation == 0 {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "process incarnation must be non-zero".to_string(),
            });
        }
        Ok(())
    }
}

/// Writes the handle marker field as the one kind, and refuses any other.
pub(crate) mod handle_kind_field {
    pub fn serialize<S: serde::Serializer>(_: &(), serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(lash_sansio::handle::HANDLE_KIND)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
        use serde::Deserialize as _;
        let kind = String::deserialize(deserializer)?;
        if kind == lash_sansio::handle::HANDLE_KIND {
            return Ok(());
        }
        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(&kind),
            &lash_sansio::handle::HANDLE_KIND,
        ))
    }
}

/// A process handle as it crosses the wire.
///
/// `handle_kind` is the ADR 0095 marker field, written and required as the one
/// kind rather than carried as a peer-supplied string: before the cutover a
/// peer could send any non-empty `handle_type` and it survived validation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessHandleView {
    #[serde(rename = "__handle__", with = "handle_kind_field")]
    #[schemars(with = "String")]
    pub handle_kind: (),
    pub id: String,
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<RemoteProcessDefinitionIdentity>,
    pub status: RemoteProcessStatus,
}

impl RemoteProcessHandleView {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "id", &self.id)?;
        require_non_empty(type_name, "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate(type_name)?;
        require_non_empty(type_name, "kind", &self.kind)?;
        if let Some(definition) = &self.definition {
            definition.validate(type_name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessRecord {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub last_event_sequence: u64,
    pub input: RemoteProcessInput,
    pub disposition: RemoteRecoveryContract,
    pub lifecycle: RemoteProcessLifecyclePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    pub identity: RemoteProcessIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
    pub provenance: RemoteProcessProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<RemoteProcessExecutionEnvRef>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<RemoteProcessExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<RemoteProcessStarted>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<RemoteAbandonRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<lash_sansio::CancelRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<RemoteProcessWaitState>,
    /// The park the process is in, while its body refuses to replay its
    /// journal (FIG-3659 NOW-B).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<RemoteProcessPark>,
    #[serde(default)]
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RemoteProcessAwaitOutput>,
}

fn validate_status_and_outcome(
    type_name: &'static str,
    status: RemoteProcessStatus,
    outcome: Option<&RemoteProcessAwaitOutput>,
) -> Result<(), RemoteProtocolError> {
    match (status.is_terminal(), outcome) {
        (false, None) => Ok(()),
        (false, Some(_)) => Err(RemoteProtocolError::InvalidEnvelope {
            type_name,
            message: format!("non-terminal process status `{status:?}` must not carry an outcome"),
        }),
        (true, None) => Err(RemoteProtocolError::InvalidEnvelope {
            type_name,
            message: format!("terminal process status `{status:?}` must carry an outcome"),
        }),
        (true, Some(outcome)) if outcome.terminal_status() != Some(status) => {
            Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!("process status `{status:?}` contradicts its outcome"),
            })
        }
        (true, Some(_)) => Ok(()),
    }
}

impl RemoteProcessRecord {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        require_storable_process_key(type_name, &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate(type_name)?;
        self.lifecycle
            .validate(type_name, &self.provenance.originator)?;
        self.input.validate(type_name)?;
        self.identity.validate(type_name)?;
        let mut event_type_names = std::collections::BTreeSet::new();
        for event_type in &self.event_types {
            event_type.validate(type_name)?;
            if !event_type_names.insert(event_type.name.as_str()) {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: format!("duplicate event type `{}`", event_type.name),
                });
            }
        }
        self.provenance.validate(type_name)?;
        if self.max_attempts == Some(0) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "max_attempts must be greater than zero".to_string(),
            });
        }
        match (self.input.requires_execution_env(), self.env_ref.is_some()) {
            (true, false) => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: "this input kind requires a captured execution env".to_string(),
                });
            }
            (false, true) => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: "this input kind must not capture an execution env".to_string(),
                });
            }
            (true, true) | (false, false) => {}
        }
        if let Some(env_ref) = &self.env_ref {
            env_ref.validate(type_name)?;
        }
        if let Some(external_ref) = &self.external_ref {
            external_ref.validate(type_name)?;
        }
        if let Some(first_started) = &self.first_started {
            first_started.owner.validate(type_name)?;
        }
        if let Some(wait) = &self.wait {
            wait.validate(type_name)?;
        }
        if let Some(park) = &self.park {
            park.validate(type_name)?;
        }
        if let Some(outcome) = &self.outcome {
            outcome.validate(type_name)?;
        }
        validate_status_and_outcome(type_name, self.status, self.outcome.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWorkSnapshot {
    pub session_id: SessionId,
    #[serde(default)]
    pub visible_processes: Vec<RemoteProcessRef>,
    #[serde(default)]
    pub items: Vec<RemoteProcessWorkItem>,
}

impl RemoteProcessWorkSnapshot {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessWorkSnapshot", "session_id", &self.session_id)?;
        for process_ref in &self.visible_processes {
            process_ref.validate("RemoteProcessWorkSnapshot")?;
        }
        for item in &self.items {
            item.validate("RemoteProcessWorkSnapshot")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWorkItem {
    pub process: RemoteObservedProcess,
    #[serde(default)]
    pub events: Vec<RemoteObservedProcessEvent>,
    pub event_tail_sequence: u64,
    pub state: RemoteObservedWorkItemState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteObservedWorkItemState {
    Coherent,
    EventTailMismatch {
        record_sequence: u64,
        event_tail_sequence: u64,
    },
}

impl RemoteProcessWorkItem {
    /// The coherence verdict the carried `process` and `events` determine.
    /// `event_tail_sequence` and `state` on the wire are a peer's re-spelling
    /// of this derivation; a payload is valid exactly when it agrees.
    fn derived_state(&self) -> RemoteObservedWorkItemState {
        let event_tail_sequence = self.events.last().map_or(0, |event| event.sequence);
        if self.process.last_event_sequence == event_tail_sequence {
            RemoteObservedWorkItemState::Coherent
        } else {
            RemoteObservedWorkItemState::EventTailMismatch {
                record_sequence: self.process.last_event_sequence,
                event_tail_sequence,
            }
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        self.process.validate(type_name)?;
        for event in &self.events {
            event.validate(type_name)?;
        }
        if self.event_tail_sequence != self.events.last().map_or(0, |event| event.sequence)
            || self.state != self.derived_state()
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "work-item event-tail sequence and state contradict the carried record and events"
                    .to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteObservedProcess {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub last_event_sequence: u64,
    pub identity: RemoteProcessIdentity,
    pub lifecycle: RemoteProcessStatus,
    /// Declared parent scope and parent-end action, distinct from the
    /// `lifecycle` status fold above.
    pub policy: RemoteProcessLifecyclePolicy,
    pub disposition: RemoteRecoveryContract,
    /// Human-readable summary of the terminal failure, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Typed classification of the same terminal failure, present exactly when
    /// `error` is. A peer that predates this field writes neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<RemoteObservedProcessFailure>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<RemoteProcessStarted>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<RemoteLeaseOwnerIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<RemoteAbandonRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<lash_sansio::CancelRequest>,
    pub input: RemoteProcessInput,
    pub originator: RemoteProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<RemoteProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<RemoteCausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<RemoteProcessExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<RemoteProcessWaitState>,
    /// The park the process is in (FIG-3659 NOW-B).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<RemoteProcessPark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<SessionId>,
}

impl RemoteObservedProcess {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate(type_name)?;
        self.identity.validate(type_name)?;
        self.policy.validate(type_name, &self.originator)?;
        self.input.validate(type_name)?;
        self.originator.validate(type_name)?;
        if let Some(lease_holder) = &self.lease_holder {
            lease_holder.validate(type_name)?;
        }
        if let Some(first_started) = &self.first_started {
            first_started.owner.validate(type_name)?;
        }
        if let Some(env_ref) = &self.env_ref {
            env_ref.validate(type_name)?;
        }
        if let Some(external_ref) = &self.external_ref {
            external_ref.validate(type_name)?;
        }
        if let Some(wait) = &self.wait {
            wait.validate(type_name)?;
        }
        if let Some(park) = &self.park {
            park.validate(type_name)?;
        }
        if let Some(child_session_id) = &self.child_session_id {
            require_non_empty(type_name, "child_session_id", child_session_id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteObservedProcessEvent {
    pub sequence: u64,
    pub event_type: String,
    pub occurred_at_ms: u64,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl RemoteObservedProcessEvent {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "event_type", &self.event_type)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEvent {
    pub process_id: ProcessId,
    pub process_incarnation: u64,
    pub sequence: u64,
    pub event_type: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<RemoteRuntimeInvocation>,
    #[serde(default)]
    pub semantics: RemoteProcessEventSemantics,
    pub occurred_at_ms: u64,
}

impl RemoteProcessEvent {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.process_incarnation,
        }
        .validate(type_name)?;
        require_non_empty(type_name, "event_type", &self.event_type)?;
        if let Some(invocation) = &self.invocation {
            invocation.validate(type_name)?;
        }
        self.semantics.validate(type_name)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventType {
    pub name: String,
    #[serde(default)]
    pub payload_schema: serde_json::Value,
    #[serde(default)]
    pub semantics: RemoteProcessEventSemanticsSpec,
}

impl RemoteProcessEventType {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "event_type.name", &self.name)?;
        self.semantics.validate(type_name)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventSemanticsSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<RemoteProcessTerminalSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<RemoteProcessWakeSpec>,
}

impl RemoteProcessEventSemanticsSpec {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Some(terminal) = &self.terminal {
            terminal.validate(type_name)?;
        }
        if let Some(wake) = &self.wake {
            wake.validate(type_name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessTerminalSpec {
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_output: Option<RemoteProcessValueSelector>,
}

impl RemoteProcessTerminalSpec {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if !self.status.is_terminal() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "terminal event semantics require a terminal status, got `{:?}`",
                    self.status
                ),
            });
        }
        if let Some(await_output) = &self.await_output {
            await_output.validate(type_name)?;
        }
        if self.status != RemoteProcessStatus::Completed && self.await_output.is_none() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message:
                    "failed, cancelled, and abandoned terminal events must declare await output"
                        .to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWakeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<RemoteProcessValueSelector>,
    pub input: RemoteProcessValueSelector,
}

impl RemoteProcessWakeSpec {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Some(when) = &self.when {
            when.validate(type_name)?;
        }
        self.input.validate(type_name)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventSemantics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<RemoteProcessTerminalSemantics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<RemoteProcessWake>,
}

impl RemoteProcessEventSemantics {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Some(terminal) = &self.terminal {
            terminal.outcome.validate(type_name)?;
            validate_status_and_outcome(type_name, terminal.status, Some(&terminal.outcome))?;
        }
        if let Some(wake) = &self.wake {
            wake.validate(type_name)?;
        }
        Ok(())
    }
}

/// Wire mirror of the producer-declared recovery contract (ADR 0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRecoveryContract {
    Rerunnable,
    OwnerBound,
    ExternallyOwned,
}

/// Wire mirror of the writer that established an Abandoned terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAbandonWriter {
    OwnerDrain,
    Sweep,
    ReconciledRequest,
    EngineGaveUp,
    ResumeRefused { reason: RemoteProcessResumeRefusal },
}

/// Wire mirror of why a started process cannot be resumed safely.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessResumeRefusal {
    RetiredGeneration { found: String },
    SubstrateLost,
}

/// Wire mirror of one lease holder incarnation. Fencing identity is the full
/// `(owner_id, incarnation_id)` pair; neither component may be erased into an
/// untyped JSON carrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteLeaseOwnerIdentity {
    pub owner_id: String,
    pub incarnation_id: String,
}

impl RemoteLeaseOwnerIdentity {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "lease_owner.owner_id", &self.owner_id)?;
        require_non_empty(
            type_name,
            "lease_owner.incarnation_id",
            &self.incarnation_id,
        )
    }
}

/// Wire mirror of Abandoned-terminal evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAbandonEvidence {
    pub writer: RemoteAbandonWriter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<RemoteLeaseOwnerIdentity>,
    pub epoch_ms: u64,
}

/// Wire mirror of the durable execution-started fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStarted {
    pub owner: RemoteLeaseOwnerIdentity,
    #[serde(default)]
    pub fencing_token: u64,
    #[serde(default = "remote_first_process_attempt")]
    pub attempt: u32,
    pub started_at_ms: u64,
    /// The replay-key grammar the incarnation was started under (FIG-3586).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_grammar: Option<u32>,
}

const fn remote_first_process_attempt() -> u32 {
    1
}

/// Wire mirror of the pending Abandon Request marker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAbandonRequest {
    pub requested_by: String,
    pub requested_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessTerminalSemantics {
    pub status: RemoteProcessStatus,
    pub outcome: RemoteProcessAwaitOutput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWake {
    pub input: String,
}

impl RemoteProcessWake {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "wake.input", &self.input)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessValueSelector {
    Payload,
    Pointer(String),
    Const(serde_json::Value),
    Template {
        template: String,
        #[serde(default)]
        fields: BTreeMap<String, RemoteProcessValueSelector>,
    },
    Present(String),
}

impl RemoteProcessValueSelector {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Payload | Self::Const(_) => Ok(()),
            Self::Pointer(pointer) => require_non_empty(type_name, "selector.pointer", pointer),
            Self::Template { template, fields } => {
                require_non_empty(type_name, "selector.template", template)?;
                for (name, selector) in fields {
                    require_non_empty(type_name, "selector.field", name)?;
                    selector.validate(type_name)?;
                }
                Ok(())
            }
            Self::Present(pointer) => require_non_empty(type_name, "selector.present", pointer),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteRuntimeInvocation {
    pub attribution: RemoteRuntimeAttribution,
    pub subject: RemoteRuntimeSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<RemoteCausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<RemoteRuntimeReplay>,
}

impl RemoteRuntimeInvocation {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        self.attribution.validate(type_name)?;
        self.subject.validate(type_name)?;
        if let Some(caused_by) = &self.caused_by {
            caused_by.validate(type_name)?;
        }
        if let Some(replay) = &self.replay {
            require_non_empty(type_name, "replay.key", &replay.key)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteRuntimeAttribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl RemoteRuntimeAttribution {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Some(session_id) = &self.session_id {
            require_non_empty(type_name, "runtime_attribution.session_id", session_id)?;
        }
        if let Some(turn_id) = &self.turn_id {
            require_non_empty(type_name, "runtime_attribution.turn_id", turn_id)?;
        }
        if self.turn_id.is_some() && self.session_id.is_none() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "runtime turn attribution requires session attribution".to_string(),
            });
        }
        if (self.turn_index.is_some() || self.protocol_iteration.is_some())
            && self.turn_id.is_none()
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "runtime progress attribution requires turn attribution".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteRuntimeReplay {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<RemoteRuntimeReplayAttribution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum RemoteRuntimeReplayAttribution {
    ToolIntent(crate::RemoteToolIntentIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteRuntimeSubject {
    Effect {
        address: lash_sansio::EffectAddress,
        effect_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay_attribution: Option<RemoteRuntimeReplayAttribution>,
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

impl RemoteRuntimeSubject {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Effect {
                address, effect_id, ..
            } => {
                address
                    .validate()
                    .map_err(|error| RemoteProtocolError::InvalidEnvelope {
                        type_name,
                        message: format!("runtime_subject.address: {error}"),
                    })?;
                require_non_empty(type_name, "runtime_subject.effect_id", effect_id)
            }
            Self::Process { process_id } => {
                require_non_empty(type_name, "runtime_subject.process_id", process_id)
            }
            Self::ProcessEvent {
                process_id,
                event_type,
                ..
            } => {
                require_non_empty(type_name, "runtime_subject.process_id", process_id)?;
                require_non_empty(type_name, "runtime_subject.event_type", event_type)
            }
            Self::TriggerOccurrence { occurrence_id, .. } => {
                require_non_empty(type_name, "runtime_subject.occurrence_id", occurrence_id)
            }
            Self::SessionNode {
                session_id,
                node_id,
            } => {
                require_non_empty(type_name, "runtime_subject.session_id", session_id)?;
                require_non_empty(type_name, "runtime_subject.node_id", node_id)
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessPluginOptions {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, serde_json::Value>,
}

fn default_remote_context_window_tokens() -> usize {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessModelLimits {
    #[serde(default = "default_remote_context_window_tokens")]
    pub context_window_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_capacity: Option<usize>,
}

impl Default for RemoteProcessModelLimits {
    fn default() -> Self {
        Self {
            context_window_tokens: default_remote_context_window_tokens(),
            output_token_capacity: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessModelSpec {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub variant: crate::llm::RemoteReasoningSelection,
    #[serde(
        default,
        skip_serializing_if = "crate::llm::RemoteModelCapability::is_empty"
    )]
    pub capability: crate::llm::RemoteModelCapability,
    #[serde(default)]
    pub limits: RemoteProcessModelLimits,
}

/// Required wire mirror of the session's turn budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessExecutionPolicy {
    #[serde(default)]
    pub model: RemoteProcessModelSpec,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub autonomous: bool,
    pub turn_budget: RemoteTurnBudget,
    #[serde(default, skip_serializing_if = "RemotePromptLayer::is_empty")]
    pub prompt: RemotePromptLayer,
    /// Session-wide generation intent, mirroring `SessionPolicy.generation`.
    /// A remote peer that persists an execution policy without it would
    /// resume the session with uncontrolled sampling.
    #[serde(
        default,
        skip_serializing_if = "crate::llm::RemoteGenerationOptions::is_empty"
    )]
    pub generation: crate::llm::RemoteGenerationOptions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessExecutionEnvSpec {
    #[serde(default, skip_serializing_if = "RemoteProcessPluginOptions::is_empty")]
    pub plugin_options: RemoteProcessPluginOptions,
    pub policy: RemoteProcessExecutionPolicy,
}

impl RemoteProcessPluginOptions {
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl RemoteProcessExecutionPolicy {
    pub fn new(turn_budget: RemoteTurnBudget) -> Self {
        Self {
            model: RemoteProcessModelSpec::default(),
            provider_id: String::new(),
            session_id: None,
            autonomous: false,
            turn_budget,
            prompt: RemotePromptLayer::default(),
            generation: crate::llm::RemoteGenerationOptions::default(),
        }
    }
}

impl RemoteProcessExecutionEnvSpec {
    pub fn new(turn_budget: RemoteTurnBudget) -> Self {
        Self {
            plugin_options: RemoteProcessPluginOptions::default(),
            policy: RemoteProcessExecutionPolicy::new(turn_budget),
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if self.policy.model.limits.context_window_tokens == 0 {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message:
                    "env_spec.policy.model.limits.context_window_tokens must be greater than zero"
                        .to_string(),
            });
        }
        if self
            .policy
            .model
            .limits
            .output_token_capacity
            .is_some_and(|value| value == 0)
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message:
                    "env_spec.policy.model.limits.output_token_capacity must be greater than zero"
                        .to_string(),
            });
        }
        self.policy.generation.validate(type_name)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePersistProcessEnvRequest {
    pub env_spec: RemoteProcessExecutionEnvSpec,
}

impl RemotePersistProcessEnvRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.env_spec.validate("RemotePersistProcessEnvRequest")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePersistProcessEnvReceipt {
    pub env_ref: RemoteProcessExecutionEnvRef,
}

impl RemotePersistProcessEnvReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.env_ref.validate("RemotePersistProcessEnvReceipt")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteProcessObserverBy {
    Host { operation_id: String },
}

impl RemoteProcessObserverBy {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Host { operation_id } => {
                require_non_empty(type_name, "observer_by.operation_id", operation_id)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStartReceipt {
    pub record: RemoteProcessRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<RemoteProcessHandleView>,
}

impl RemoteProcessStartReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.record.validate("RemoteProcessStartReceipt")?;
        if let Some(summary) = &self.summary {
            summary.validate("RemoteProcessStartReceipt")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessStatusFilter {
    Any,
    In(std::collections::BTreeSet<RemoteProcessStatus>),
}

impl Default for RemoteProcessStatusFilter {
    fn default() -> Self {
        Self::In(std::collections::BTreeSet::from([
            RemoteProcessStatus::Running,
        ]))
    }
}

impl RemoteProcessStatusFilter {
    pub fn any_of(statuses: impl IntoIterator<Item = RemoteProcessStatus>) -> Self {
        Self::In(statuses.into_iter().collect())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessListFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<serde_json::Value>,
    #[serde(default)]
    pub status: RemoteProcessStatusFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originator: Option<RemoteProcessOriginatorFilter>,
    /// Selects the children of one durable parent scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_scope: Option<RemoteParentScope>,
    /// Selects nonterminal rows whose cancellation request predates this
    /// timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_pending_before_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by_occurrence_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by_subscription_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_end_ms: Option<u64>,
    /// Inclusive lower bound for retired-process update timestamps. Live
    /// processes remain eligible regardless of age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_since_ms: Option<u64>,
}

impl RemoteProcessListFilter {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        if self
            .definition
            .as_ref()
            .is_some_and(serde_json::Value::is_null)
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessListFilter",
                message: "definition value cannot be null".to_string(),
            });
        }
        if let Some(originator) = &self.originator {
            originator.validate("RemoteProcessListFilter")?;
        }
        Ok(())
    }
}

/// Wire mirror of the typed originator selector.
///
/// A `Session` selector with no `agent_frame_id` selects every process the
/// session started; one naming a frame selects only that frame's processes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessOriginatorFilter {
    Host {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Session(RemoteSessionScope),
}

impl RemoteProcessOriginatorFilter {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Host { .. } => Ok(()),
            Self::Session(scope) => scope.validate(type_name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessListResponse {
    #[serde(default)]
    pub records: Vec<RemoteObservedProcess>,
}

impl RemoteProcessListResponse {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        for record in &self.records {
            record.validate("RemoteProcessListResponse")?;
        }
        Ok(())
    }
}
