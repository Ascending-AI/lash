/// Stable runtime error code.
///
/// Codes serialize as the same snake_case strings exposed in traces and host
/// errors, but callers should match this type instead of parsing display text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeErrorCode {
    MissingExecutionScopeId,
    ExecutionScopeTurnIdMismatch,
    SessionExecutionBusy,
    SessionExecutionLeaseLost,
    /// The store aborted a commit before publication because transactional
    /// write authority was contended. Retrying the same operation unchanged is
    /// safe; reloading or rebasing is not required.
    StoreCommitContended,
    /// The final runtime commit contains more graph nodes than the shared
    /// transaction budget permits. The same turn will fail identically until
    /// the host produces a smaller turn.
    StoreCommitNodeBudgetExceeded,
    /// The final runtime commit contains more persisted payload bytes than the
    /// shared transaction budget permits. The same turn will fail identically
    /// until the host produces a smaller turn.
    StoreCommitByteBudgetExceeded,
    /// A process (re-)execution was handed an empty/non-persisted process id.
    /// Process execution identity is the persisted `process_id`; a retry that
    /// cannot present that stable id has lost its idempotency anchor.
    MissingProcessExecutionId,
    StoreCommitFailed,
    PluginSessionManager,
    PluginFinalizeTurn,
    PluginCheckpoint,
    PluginPrepareTurn,
    ContextPrepareTurn,
    ProtocolTurnExtension,
    ProtocolBeforeLlmCall,
    TurnStreamJoin,
    EmptyAgentFrameRun,
    DurableEffectLiveProtocolExtension,
    DurableEffectLivePluginInput,
    Other(String),
}

impl RuntimeErrorCode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::MissingExecutionScopeId => "missing_execution_scope_id",
            Self::ExecutionScopeTurnIdMismatch => "execution_scope_turn_id_mismatch",
            Self::SessionExecutionBusy => "session_execution_busy",
            Self::SessionExecutionLeaseLost => "session_execution_lease_lost",
            Self::StoreCommitContended => "store_commit_contended",
            Self::StoreCommitNodeBudgetExceeded => "store_commit_node_budget_exceeded",
            Self::StoreCommitByteBudgetExceeded => "store_commit_byte_budget_exceeded",
            Self::MissingProcessExecutionId => "missing_process_execution_id",
            Self::StoreCommitFailed => "store_commit_failed",
            Self::PluginSessionManager => "plugin_session_manager",
            Self::PluginFinalizeTurn => "plugin_finalize_turn",
            Self::PluginCheckpoint => "plugin_checkpoint",
            Self::PluginPrepareTurn => "plugin_prepare_turn",
            Self::ContextPrepareTurn => "context_prepare_turn",
            Self::ProtocolTurnExtension => "protocol_turn_extension",
            Self::ProtocolBeforeLlmCall => "protocol_before_llm_call",
            Self::TurnStreamJoin => "turn_stream_join",
            Self::EmptyAgentFrameRun => "empty_agent_frame_run",
            Self::DurableEffectLiveProtocolExtension => "durable_effect_live_protocol_extension",
            Self::DurableEffectLivePluginInput => "durable_effect_live_plugin_input",
            Self::Other(code) => code.as_str(),
        }
    }
}

impl std::fmt::Display for RuntimeErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for RuntimeErrorCode {
    fn from(code: &str) -> Self {
        match code {
            "missing_execution_scope_id" => Self::MissingExecutionScopeId,
            "execution_scope_turn_id_mismatch" => Self::ExecutionScopeTurnIdMismatch,
            "session_execution_busy" => Self::SessionExecutionBusy,
            "session_execution_lease_lost" => Self::SessionExecutionLeaseLost,
            "store_commit_contended" => Self::StoreCommitContended,
            "store_commit_node_budget_exceeded" => Self::StoreCommitNodeBudgetExceeded,
            "store_commit_byte_budget_exceeded" => Self::StoreCommitByteBudgetExceeded,
            "missing_process_execution_id" => Self::MissingProcessExecutionId,
            "store_commit_failed" => Self::StoreCommitFailed,
            "plugin_session_manager" => Self::PluginSessionManager,
            "plugin_finalize_turn" => Self::PluginFinalizeTurn,
            "plugin_checkpoint" => Self::PluginCheckpoint,
            "plugin_prepare_turn" => Self::PluginPrepareTurn,
            "context_prepare_turn" => Self::ContextPrepareTurn,
            "protocol_turn_extension" => Self::ProtocolTurnExtension,
            "protocol_before_llm_call" => Self::ProtocolBeforeLlmCall,
            "turn_stream_join" => Self::TurnStreamJoin,
            "empty_agent_frame_run" => Self::EmptyAgentFrameRun,
            "durable_effect_live_protocol_extension" => Self::DurableEffectLiveProtocolExtension,
            "durable_effect_live_plugin_input" => Self::DurableEffectLivePluginInput,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for RuntimeErrorCode {
    fn from(code: String) -> Self {
        Self::from(code.as_str())
    }
}

impl serde::Serialize for RuntimeErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for RuntimeErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code = <String as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::from(code))
    }
}

/// Runtime error for unexpected failures.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeError {
    pub code: RuntimeErrorCode,
    pub message: String,
}

impl RuntimeError {
    pub fn new(code: impl Into<RuntimeErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn is_code(&self, code: RuntimeErrorCode) -> bool {
        self.code == code
    }

    /// Build the loud error raised when a process (re-)execution is handed an
    /// empty/non-persisted id.
    ///
    /// Process execution identity is the persisted `process_id`, so a retry
    /// must present that stable id — mirroring how
    /// [`ExecutionScope`](crate::ExecutionScope) rejects an empty stable id.
    pub fn missing_process_execution_id() -> Self {
        Self::new(
            RuntimeErrorCode::MissingProcessExecutionId,
            "process execution requires a non-empty persisted process id",
        )
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::{RuntimeError, RuntimeErrorCode};

    #[test]
    fn missing_process_execution_id_round_trips() {
        let err = RuntimeError::missing_process_execution_id();
        assert_eq!(err.code, RuntimeErrorCode::MissingProcessExecutionId);
        let json = serde_json::to_value(&err).expect("serialize runtime error");
        assert_eq!(json["code"], "missing_process_execution_id");
        let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
        assert_eq!(decoded.code, RuntimeErrorCode::MissingProcessExecutionId);
    }

    #[test]
    fn session_execution_lease_lost_round_trips() {
        let err = RuntimeError::new(RuntimeErrorCode::SessionExecutionLeaseLost, "lease lost");
        let json = serde_json::to_value(&err).expect("serialize runtime error");
        assert_eq!(json["code"], "session_execution_lease_lost");
        let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
        assert_eq!(decoded.code, RuntimeErrorCode::SessionExecutionLeaseLost);
    }

    #[test]
    fn runtime_error_code_serializes_as_stable_string() {
        let err = RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, "commit failed");

        let json = serde_json::to_value(&err).expect("serialize runtime error");
        assert_eq!(json["code"], "store_commit_failed");

        let decoded: RuntimeError = serde_json::from_value(json).expect("decode runtime error");
        assert_eq!(decoded.code, RuntimeErrorCode::StoreCommitFailed);
    }

    #[test]
    fn unknown_runtime_error_code_round_trips() {
        let decoded: RuntimeError = serde_json::from_value(serde_json::json!({
            "code": "plugin_defined_abort",
            "message": "stopped by plugin"
        }))
        .expect("decode plugin runtime error");

        assert_eq!(
            decoded.code,
            RuntimeErrorCode::Other("plugin_defined_abort".to_string())
        );
        assert_eq!(decoded.code.as_str(), "plugin_defined_abort");
    }
}
