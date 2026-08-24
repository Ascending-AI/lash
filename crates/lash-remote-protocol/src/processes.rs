//! Process lifecycle envelopes: start/cancel/signal/await/list requests and
//! results, process records and summaries, event semantics, execution
//! environments, and runtime invocation provenance.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::prompt::RemotePromptLayer;
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::tools::RemoteToolOutputContract;
use crate::turn_input::RemoteTurnInput;
use crate::turn_result::RemoteCausalRef;
use crate::{REMOTE_PROTOCOL_VERSION, ensure_protocol_version};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSessionScope {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_id: Option<String>,
}

impl RemoteSessionScope {
    pub fn new(session_id: impl Into<String>) -> Self {
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
    pub const PREFIX: &'static str = "process-env:v3:sha256:";

    pub fn parse(value: impl Into<String>) -> Result<Self, RemoteProtocolError> {
        let value = value.into();
        if is_canonical_process_execution_env_ref(&value) {
            Ok(Self(value))
        } else {
            Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessExecutionEnvRef",
                message: "env_ref must match `process-env:v3:sha256:<64 lowercase hex>`"
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
                message: "env_ref must match `process-env:v3:sha256:<64 lowercase hex>`"
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
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_frame_id: Option<String>,
    },
}

impl RemoteProcessOriginator {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Host { .. } => Ok(()),
            Self::Session { session_id, .. } => {
                require_non_empty(type_name, "session_id", session_id)
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
        self.originator.validate(type_name)
    }
}
#[rustfmt::skip] #[cfg(all(test, feature = "core-conversions"))] mod core_process_status_label_tests { include!("core_process_status_label_tests.rs"); }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessDefinitionIdentity {
    #[serde(default)]
    pub value: serde_json::Value,
}

impl RemoteProcessDefinitionIdentity {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if self.value.is_null() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: "definition value cannot be null".to_string(),
            });
        }
        Ok(())
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
            Self::ToolCall {
                prepared_tool_call: _,
            } => Ok(()),
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Abandoned
        )
    }

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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteProcessAwaitOutput {
    Success {
        value: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
    Failure {
        class: RemoteToolFailureClass,
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
    Cancelled {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
    Abandoned {
        evidence: RemoteAbandonEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<serde_json::Value>,
    },
    NoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
}

impl RemoteProcessAwaitOutput {
    fn terminal_status(&self) -> Option<RemoteProcessStatus> {
        match self {
            Self::Success { .. } => Some(RemoteProcessStatus::Completed),
            Self::Failure { .. } => Some(RemoteProcessStatus::Failed),
            Self::Cancelled { .. } => Some(RemoteProcessStatus::Cancelled),
            Self::Abandoned { .. } => Some(RemoteProcessStatus::Abandoned),
            Self::NoLongerRetained { .. } => None,
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Success { .. } | Self::Abandoned { .. } | Self::NoLongerRetained { .. } => Ok(()),
            Self::Failure { code, message, .. } => {
                require_non_empty(type_name, "await_output.code", code)?;
                require_non_empty(type_name, "await_output.message", message)
            }
            Self::Cancelled { message, .. } => {
                require_non_empty(type_name, "await_output.message", message)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolFailureClass {
    InvalidRequest,
    Io,
    Unavailable,
    PermissionDenied,
    Timeout,
    Execution,
    External,
    ResourceLimit,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessExternalRef {
    pub backend: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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
pub struct RemoteProcessHandleView {
    #[serde(rename = "__handle__")]
    pub handle_type: String,
    pub id: String,
    pub process_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<RemoteProcessDefinitionIdentity>,
    pub status: RemoteProcessStatus,
}

impl RemoteProcessHandleView {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "handle_type", &self.handle_type)?;
        require_non_empty(type_name, "id", &self.id)?;
        require_non_empty(type_name, "process_id", &self.process_id)?;
        require_non_empty(type_name, "kind", &self.kind)?;
        if let Some(definition) = &self.definition {
            definition.validate(type_name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessRecord {
    pub process_id: String,
    pub input: RemoteProcessInput,
    pub disposition: RemoteRecoveryContract,
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
    pub wait: Option<RemoteProcessWaitState>,
    #[serde(default)]
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RemoteProcessAwaitOutput>,
}

impl RemoteProcessRecord {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        self.input.validate(type_name)?;
        self.identity.validate(type_name)?;
        for event_type in &self.event_types {
            event_type.validate(type_name)?;
        }
        self.provenance.validate(type_name)?;
        if let Some(env_ref) = &self.env_ref {
            env_ref.validate(type_name)?;
        }
        if let Some(external_ref) = &self.external_ref {
            external_ref.validate(type_name)?;
        }
        if let Some(wait) = &self.wait {
            wait.validate(type_name)?;
        }
        if let Some(outcome) = &self.outcome {
            outcome.validate(type_name)?;
        }
        match (self.status.is_terminal(), self.outcome.as_ref()) {
            (false, None) => {}
            (false, Some(_)) => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: format!(
                        "non-terminal process status `{:?}` must not carry an outcome",
                        self.status
                    ),
                });
            }
            (true, None) => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: format!(
                        "terminal process status `{:?}` must carry an outcome",
                        self.status
                    ),
                });
            }
            (true, Some(outcome)) if outcome.terminal_status() != Some(self.status) => {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: format!("process status `{:?}` contradicts its outcome", self.status),
                });
            }
            (true, Some(_)) => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessWorkSnapshot {
    pub protocol_version: u32,
    pub session_id: String,
    #[serde(default)]
    pub visible_process_ids: Vec<String>,
    #[serde(default)]
    pub items: Vec<RemoteProcessWorkItem>,
}

impl RemoteProcessWorkSnapshot {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessWorkSnapshot", "session_id", &self.session_id)?;
        for process_id in &self.visible_process_ids {
            require_non_empty(
                "RemoteProcessWorkSnapshot",
                "visible_process_ids",
                process_id,
            )?;
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
    pub kind: String,
    pub label: String,
}

impl RemoteProcessWorkItem {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        self.process.validate(type_name)?;
        for event in &self.events {
            event.validate(type_name)?;
        }
        require_non_empty(type_name, "kind", &self.kind)?;
        if self.kind != self.process.identity.kind {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "work-item kind `{}` contradicts process identity kind `{}`",
                    self.kind, self.process.identity.kind
                ),
            });
        }
        require_non_empty(type_name, "label", &self.label)?;
        let expected_label = self
            .process
            .identity
            .label
            .as_deref()
            .unwrap_or(self.process.identity.kind.as_str());
        if self.label != expected_label {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "work-item label `{}` contradicts process identity; expected `{expected_label}`",
                    self.label
                ),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteObservedProcess {
    pub process_id: String,
    pub graph_key: String,
    pub kind: String,
    pub identity: RemoteProcessIdentity,
    pub lifecycle: RemoteProcessStatus,
    pub status_label: String,
    pub terminal: bool,
    pub disposition: RemoteRecoveryContract,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<RemoteProcessStarted>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<RemoteAbandonRequest>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    pub label: String,
}

impl RemoteObservedProcess {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "process_id", &self.process_id)?;
        require_non_empty(type_name, "graph_key", &self.graph_key)?;
        let expected_graph_key = format!("process:{}", self.process_id);
        if self.graph_key != expected_graph_key {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "process graph key `{}` contradicts process id `{}`; expected `{expected_graph_key}`",
                    self.graph_key, self.process_id
                ),
            });
        }
        require_non_empty(type_name, "kind", &self.kind)?;
        self.identity.validate(type_name)?;
        if self.kind != self.identity.kind {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "process kind `{}` contradicts identity kind `{}`",
                    self.kind, self.identity.kind
                ),
            });
        }
        require_non_empty(type_name, "label", &self.label)?;
        let expected_label = self
            .identity
            .label
            .as_deref()
            .unwrap_or(self.identity.kind.as_str());
        if self.label != expected_label {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "process label `{}` contradicts identity; expected `{expected_label}`",
                    self.label
                ),
            });
        }
        require_non_empty(type_name, "status_label", &self.status_label)?;
        if self.status_label != self.lifecycle.label() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "process status label `{}` contradicts lifecycle `{:?}`; expected `{}`",
                    self.status_label,
                    self.lifecycle,
                    self.lifecycle.label()
                ),
            });
        }
        if self.terminal != self.lifecycle.is_terminal() {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name,
                message: format!(
                    "process terminal flag `{}` contradicts lifecycle `{:?}`",
                    self.terminal, self.lifecycle
                ),
            });
        }
        self.input.validate(type_name)?;
        self.originator.validate(type_name)?;
        if let Some(env_ref) = &self.env_ref {
            env_ref.validate(type_name)?;
        }
        if let Some(external_ref) = &self.external_ref {
            external_ref.validate(type_name)?;
        }
        if let Some(wait) = &self.wait {
            wait.validate(type_name)?;
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
pub struct RemoteProcessEvent {
    pub process_id: String,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAbandonWriter {
    OwnerDrain,
    Sweep,
    ReconciledRequest,
    EngineGaveUp,
}

/// Wire mirror of Abandoned-terminal evidence. The dead/lapsed owner identity is
/// carried opaquely (the wire has no lease-owner mirror), matching how other
/// nested core types ride the wire as JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAbandonEvidence {
    pub writer: RemoteAbandonWriter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<serde_json::Value>,
    pub epoch_ms: u64,
}

/// Wire mirror of the durable execution-started fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStarted {
    #[serde(default)]
    pub owner: serde_json::Value,
    #[serde(default)]
    pub fencing_token: u64,
    #[serde(default = "remote_first_process_attempt")]
    pub attempt: u32,
    pub started_at_ms: u64,
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
    pub scope: RemoteRuntimeScope,
    pub subject: RemoteRuntimeSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<RemoteCausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<RemoteRuntimeReplay>,
}

impl RemoteRuntimeInvocation {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        self.scope.validate(type_name)?;
        self.subject.validate(type_name)?;
        if let Some(replay) = &self.replay {
            require_non_empty(type_name, "replay.key", &replay.key)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteRuntimeScope {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl RemoteRuntimeScope {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        require_non_empty(type_name, "runtime_scope.session_id", &self.session_id)?;
        if let Some(turn_id) = &self.turn_id {
            require_non_empty(type_name, "runtime_scope.turn_id", turn_id)?;
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
        effect_id: String,
        kind: RemoteRuntimeEffectKind,
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

impl RemoteRuntimeSubject {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Effect { effect_id, .. } => {
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
            Self::TriggerOccurrence { occurrence_id } => {
                require_non_empty(type_name, "runtime_subject.occurrence_id", occurrence_id)
            }
            Self::SessionNode { node_id } => {
                require_non_empty(type_name, "runtime_subject.node_id", node_id)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRuntimeEffectKind {
    LlmCall,
    AssistantResponseHooks,
    Direct,
    ToolAttempt,
    ToolBatch,
    ToolParentEnd,
    Process,
    Trigger,
    ExecCode,
    AcceptTurnInput,
    Checkpoint,
    SyncExecutionEnvironment,
    Sleep,
    AwaitEvent,
    PeekAwaitEvent,
    LanguageRuntimeValue,
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
    pub session_id: Option<String>,
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
    pub protocol_version: u32,
    pub env_spec: RemoteProcessExecutionEnvSpec,
}

impl RemotePersistProcessEnvRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        self.env_spec.validate("RemotePersistProcessEnvRequest")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePersistProcessEnvReceipt {
    pub protocol_version: u32,
    pub env_ref: RemoteProcessExecutionEnvRef,
}

impl RemotePersistProcessEnvReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        self.env_ref.validate("RemotePersistProcessEnvReceipt")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteProcessObserverBy {
    Host { operation_id: String },
    ForkInheritance,
}

impl RemoteProcessObserverBy {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        match self {
            Self::Host { operation_id } => {
                require_non_empty(type_name, "observer_by.operation_id", operation_id)
            }
            Self::ForkInheritance => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteObserverInheritance {
    All,
    None,
    Only(Vec<String>),
}

impl RemoteObserverInheritance {
    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Self::Only(process_ids) = self {
            for process_id in process_ids {
                require_non_empty(type_name, "observer_inheritance.only", process_id)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStartRequest {
    pub protocol_version: u32,
    pub id: String,
    pub input: RemoteProcessInput,
    pub disposition: RemoteRecoveryContract,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<RemoteProcessExecutionEnvSpec>,
    pub originator: RemoteProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<RemoteProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
}

impl RemoteProcessStartRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessStartRequest", "id", &self.id)?;
        if self.max_attempts == Some(0) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessStartRequest",
                message: "max_attempts must be greater than zero when provided".to_string(),
            });
        }
        self.input.validate("RemoteProcessStartRequest")?;
        if let Some(env_spec) = &self.env_spec {
            env_spec.validate("RemoteProcessStartRequest")?;
        }
        if let Some(identity) = &self.identity {
            identity.validate("RemoteProcessStartRequest")?;
        }
        if let RemoteProcessInput::SessionTurn { turn_input, .. } = &self.input
            && turn_input.protocol_version != self.protocol_version
        {
            return Err(RemoteProtocolError::MismatchedNestedProtocolVersion {
                parent: "RemoteProcessStartRequest",
                child: "input.turn_input",
                parent_version: self.protocol_version,
                child_version: turn_input.protocol_version,
            });
        }
        self.originator.validate("RemoteProcessStartRequest")?;
        if let Some(wake_session_id) = &self.wake_session_id {
            require_non_empty(
                "RemoteProcessStartRequest",
                "wake_session_id",
                wake_session_id,
            )?;
        }
        for observer in &self.observers {
            require_non_empty("RemoteProcessStartRequest", "observers", observer)?;
        }
        for event_type in &self.event_types {
            event_type.validate("RemoteProcessStartRequest")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStartReceipt {
    pub protocol_version: u32,
    pub record: RemoteProcessRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<RemoteProcessHandleView>,
}

impl RemoteProcessStartReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        self.record.validate("RemoteProcessStartReceipt")?;
        if let Some(summary) = &self.summary {
            summary.validate("RemoteProcessStartReceipt")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProcessStatusFilter {
    #[default]
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    CallerDeparted,
    Any,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessListFilter {
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<RemoteProcessDefinitionIdentity>,
    #[serde(default)]
    pub status: RemoteProcessStatusFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originator_id: Option<String>,
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
}

impl Default for RemoteProcessListFilter {
    fn default() -> Self {
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            definition: None,
            status: RemoteProcessStatusFilter::Running,
            waiting: None,
            originator_id: None,
            identity_kind: None,
            identity_label: None,
            caused_by_occurrence_id: None,
            caused_by_subscription_id: None,
            created_at_start_ms: None,
            created_at_end_ms: None,
        }
    }
}

impl RemoteProcessListFilter {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        if let Some(definition) = &self.definition {
            definition.validate("RemoteProcessListFilter")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessListResponse {
    pub protocol_version: u32,
    #[serde(default)]
    pub records: Vec<RemoteObservedProcess>,
}

impl RemoteProcessListResponse {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        for record in &self.records {
            record.validate("RemoteProcessListResponse")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelRequest {
    pub protocol_version: u32,
    pub process_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RemoteProcessCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessCancelRequest", "process_id", &self.process_id)?;
        if let Some(reason) = &self.reason {
            require_non_empty("RemoteProcessCancelRequest", "reason", reason)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelReceipt {
    pub protocol_version: u32,
    pub process_id: String,
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RemoteProcessRecord>,
}

impl RemoteProcessCancelReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessCancelReceipt", "process_id", &self.process_id)?;
        if let Some(record) = &self.record {
            record.validate("RemoteProcessCancelReceipt")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessSignalRequest {
    pub protocol_version: u32,
    pub process_id: String,
    pub signal_name: String,
    pub signal_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<String>,
}

impl RemoteProcessSignalRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessSignalRequest", "process_id", &self.process_id)?;
        require_non_empty(
            "RemoteProcessSignalRequest",
            "signal_name",
            &self.signal_name,
        )?;
        require_non_empty("RemoteProcessSignalRequest", "signal_id", &self.signal_id)?;
        if let Some(replay_key) = &self.replay_key {
            require_non_empty("RemoteProcessSignalRequest", "replay_key", replay_key)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessSignalReceipt {
    pub protocol_version: u32,
    pub event: RemoteProcessEvent,
}

impl RemoteProcessSignalReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        self.event.validate("RemoteProcessSignalReceipt")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessAwaitRequest {
    pub protocol_version: u32,
    pub process_id: String,
}

impl RemoteProcessAwaitRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessAwaitRequest", "process_id", &self.process_id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessAwaitOutcome {
    pub protocol_version: u32,
    pub process_id: String,
    pub output: RemoteProcessAwaitOutput,
}

impl RemoteProcessAwaitOutcome {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessAwaitOutcome", "process_id", &self.process_id)?;
        self.output.validate("RemoteProcessAwaitOutcome")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventsRequest {
    pub protocol_version: u32,
    pub process_id: String,
    #[serde(default)]
    pub after_sequence: u64,
}

impl RemoteProcessEventsRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteProcessEventsRequest", "process_id", &self.process_id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventsResponse {
    pub protocol_version: u32,
    pub process_id: String,
    #[serde(default)]
    pub events: Vec<RemoteProcessEvent>,
}

impl RemoteProcessEventsResponse {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty(
            "RemoteProcessEventsResponse",
            "process_id",
            &self.process_id,
        )?;
        for event in &self.events {
            event.validate("RemoteProcessEventsResponse")?;
        }
        Ok(())
    }
}
