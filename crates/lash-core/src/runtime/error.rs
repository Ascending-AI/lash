/// Stable runtime error code.
///
/// Codes serialize as the same snake_case strings exposed in traces and host
/// errors, but callers should match this type instead of parsing display text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuntimeErrorCode {
    MissingExecutionScopeId,
    ExecutionScopeTurnIdMismatch,
    SessionExecutionBusy,
    /// The managed-turn registry's admission cap is full. Retrying the
    /// same request after another managed turn finishes is safe.
    ManagedTurnConcurrencyLimitExceeded,
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
    /// Dirty executor state could not be captured before commit. No store
    /// publication was attempted; the live lease and claims are released.
    ExecutionStateCaptureFailed,
    /// Resident plugin/protocol state was invalidated after a committed turn.
    /// Every subsequent resident-state consumer fails with this code until a
    /// durable reload succeeds. A deterministic restore fault therefore keeps
    /// returning this error; retry after repairing the cause or cold-open a new
    /// handle from the durable state.
    ResidentSessionReloadFailed,
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
    AwaitEventCancelUnsupported,
    AwaitEventKeySign,
    AwaitEventUnknownOrRevoked,
    AwaitEventUnsupported,
    CancelStartGateUnavailable,
    EffectJournalRetirementUnsupported,
    InvalidAwaitEventSessionId,
    InvalidAwaitEventWaitIdentity,
    InvalidTurnCancelRequest,
    LiveReplay,
    LlmProvider,
    PostgresAwaitEventDecode,
    PostgresAwaitEventEncode,
    /// Process-local; repaired by restart, not by same-process retry.
    PostgresAwaitEventNotify,
    PostgresAwaitEventSign,
    PostgresAwaitEventStore,
    PostgresEffectJournalRetirement,
    QueuedWork,
    RestateAwaitEventAwait,
    RestateAwaitEventCancel,
    /// The local observer was cancelled while the durable promise stayed live;
    /// its disposition is unknown until another observer attaches.
    RestateAwaitEventCancelled,
    RestateAwaitEventPeek,
    RestateAwaitEventResolve,
    RestateAwaitEventRevocationRead,
    RestateAwaitEventRevoke,
    RestateAwaitEventSessionUpdate,
    RestateEffectController,
    RestateProcessTerminalEncode,
    RestateSegmentProgramHashMismatch,
    RestateTurnTerminalAttach,
    /// A bounded Restate terminal attachment elapsed while the durable wait
    /// remained live. Re-attaching the same address is explicitly safe.
    RestateTurnTerminalAttachCeilingElapsed,
    RestateTurnTerminalDecode,
    RestateTurnTerminalInvalidResolution,
    /// Process-local; repaired by restart, not by same-process retry.
    RuntimeEffectControllerTaskClosed,
    RuntimePerfStartGateRetry,
    RuntimeStore,
    /// Durable state is corrupt or an authoritative monotonic counter has
    /// exhausted its representable domain. Retrying unchanged cannot heal it.
    RuntimeStoreCorrupt,
    SessionCommandClaim,
    SessionCommandIdempotencyKey,
    SessionCommandPostDriveRefresh,
    SessionCommandRefresh,
    SessionCommandRefreshTools,
    SessionDeleteScopeMismatch,
    SessionHeadRefresh,
    SessionToolRegistry,
    SqliteAwaitEventDecode,
    SqliteAwaitEventEncode,
    /// Process-local; repaired by restart, not by same-process retry.
    SqliteAwaitEventNotify,
    SqliteAwaitEventSign,
    SqliteAwaitEventStore,
    SqliteEffectJournalRetirement,
    ToolCompletionKeyMissingCallId,
    ToolCompletionKeyProcessLifetime,
    TransientCancelWatch,
    TransientTerminalPublication,
    TurnCancelGateDecode,
    TurnCancelGateEncode,
    TurnCancelGateInvalidTerminal,
    TurnCancellationEvidenceMissing,
    TurnControlPeekOutcome,
    TurnControlUnknownOrRevoked,
    /// The local observer was cancelled while the durable promise stayed live;
    /// its disposition is unknown until another observer attaches.
    TurnControlWaitCancelled,
    TurnControlWaitTimeout,
    TurnTerminalAwaitTimeout,
    TurnTerminalDecode,
    TurnTerminalEncode,
    TurnTerminalInvalidResolution,
    TurnTerminalUnknownOrRevoked,
    /// A code minted by a public plugin or effect-host extension point.
    ///
    /// Built-in `RuntimeError` constructors use typed variants; open plugin and
    /// effect-controller boundaries use this for host-defined or controller-local
    /// codes. Extensions must namespace codes and avoid built-in `as_str` values.
    /// Foreign codes are conservatively neither retryable nor terminal.
    #[non_exhaustive]
    ForeignCode(String),
}

impl RuntimeErrorCode {
    /// Provides the canonical str view to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn as_str(&self) -> &str {
        match self {
            Self::MissingExecutionScopeId => "missing_execution_scope_id",
            Self::ExecutionScopeTurnIdMismatch => "execution_scope_turn_id_mismatch",
            Self::SessionExecutionBusy => "session_execution_busy",
            Self::ManagedTurnConcurrencyLimitExceeded => "managed_turn_concurrency_limit_exceeded",
            Self::SessionExecutionLeaseLost => "session_execution_lease_lost",
            Self::StoreCommitContended => "store_commit_contended",
            Self::StoreCommitNodeBudgetExceeded => "store_commit_node_budget_exceeded",
            Self::StoreCommitByteBudgetExceeded => "store_commit_byte_budget_exceeded",
            Self::MissingProcessExecutionId => "missing_process_execution_id",
            Self::ExecutionStateCaptureFailed => "execution_state_capture_failed",
            Self::ResidentSessionReloadFailed => "resident_session_reload_failed",
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
            Self::AwaitEventCancelUnsupported => "await_event_cancel_unsupported",
            Self::AwaitEventKeySign => "await_event_key_sign",
            Self::AwaitEventUnknownOrRevoked => "await_event_unknown_or_revoked",
            Self::AwaitEventUnsupported => "await_event_unsupported",
            Self::CancelStartGateUnavailable => "cancel_start_gate_unavailable",
            Self::EffectJournalRetirementUnsupported => "effect_journal_retirement_unsupported",
            Self::InvalidAwaitEventSessionId => "invalid_await_event_session_id",
            Self::InvalidAwaitEventWaitIdentity => "invalid_await_event_wait_identity",
            Self::InvalidTurnCancelRequest => "invalid_turn_cancel_request",
            Self::LiveReplay => "live_replay",
            Self::LlmProvider => "llm_provider",
            Self::PostgresAwaitEventDecode => "postgres_await_event_decode",
            Self::PostgresAwaitEventEncode => "postgres_await_event_encode",
            Self::PostgresAwaitEventNotify => "postgres_await_event_notify",
            Self::PostgresAwaitEventSign => "postgres_await_event_sign",
            Self::PostgresAwaitEventStore => "postgres_await_event_store",
            Self::PostgresEffectJournalRetirement => "postgres_effect_journal_retirement",
            Self::QueuedWork => "queued_work",
            Self::RestateAwaitEventAwait => "restate_await_event_await",
            Self::RestateAwaitEventCancel => "restate_await_event_cancel",
            Self::RestateAwaitEventCancelled => "restate_await_event_cancelled",
            Self::RestateAwaitEventPeek => "restate_await_event_peek",
            Self::RestateAwaitEventResolve => "restate_await_event_resolve",
            Self::RestateAwaitEventRevocationRead => "restate_await_event_revocation_read",
            Self::RestateAwaitEventRevoke => "restate_await_event_revoke",
            Self::RestateAwaitEventSessionUpdate => "restate_await_event_session_update",
            Self::RestateEffectController => "restate_effect_controller",
            Self::RestateProcessTerminalEncode => "restate_process_terminal_encode",
            Self::RestateSegmentProgramHashMismatch => "restate_segment_program_hash_mismatch",
            Self::RestateTurnTerminalAttach => "restate_turn_terminal_attach",
            Self::RestateTurnTerminalAttachCeilingElapsed => {
                "restate_turn_terminal_attach_ceiling_elapsed"
            }
            Self::RestateTurnTerminalDecode => "restate_turn_terminal_decode",
            Self::RestateTurnTerminalInvalidResolution => {
                "restate_turn_terminal_invalid_resolution"
            }
            Self::RuntimeEffectControllerTaskClosed => "runtime_effect_controller_task_closed",
            Self::RuntimePerfStartGateRetry => "runtime_perf_start_gate_retry",
            Self::RuntimeStore => "runtime_store",
            Self::RuntimeStoreCorrupt => "runtime_store_corrupt",
            Self::SessionCommandClaim => "session_command_claim",
            Self::SessionCommandIdempotencyKey => "session_command_idempotency_key",
            Self::SessionCommandPostDriveRefresh => "session_command_post_drive_refresh",
            Self::SessionCommandRefresh => "session_command_refresh",
            Self::SessionCommandRefreshTools => "session_command_refresh_tools",
            Self::SessionDeleteScopeMismatch => "session_delete_scope_mismatch",
            Self::SessionHeadRefresh => "session_head_refresh",
            Self::SessionToolRegistry => "session_tool_registry",
            Self::SqliteAwaitEventDecode => "sqlite_await_event_decode",
            Self::SqliteAwaitEventEncode => "sqlite_await_event_encode",
            Self::SqliteAwaitEventNotify => "sqlite_await_event_notify",
            Self::SqliteAwaitEventSign => "sqlite_await_event_sign",
            Self::SqliteAwaitEventStore => "sqlite_await_event_store",
            Self::SqliteEffectJournalRetirement => "sqlite_effect_journal_retirement",
            Self::ToolCompletionKeyMissingCallId => "tool_completion_key_missing_call_id",
            Self::ToolCompletionKeyProcessLifetime => "tool_completion_key_process_lifetime",
            Self::TransientCancelWatch => "transient_cancel_watch",
            Self::TransientTerminalPublication => "transient_terminal_publication",
            Self::TurnCancelGateDecode => "turn_cancel_gate_decode",
            Self::TurnCancelGateEncode => "turn_cancel_gate_encode",
            Self::TurnCancelGateInvalidTerminal => "turn_cancel_gate_invalid_terminal",
            Self::TurnCancellationEvidenceMissing => "turn_cancellation_evidence_missing",
            Self::TurnControlPeekOutcome => "turn_control_peek_outcome",
            Self::TurnControlUnknownOrRevoked => "turn_control_unknown_or_revoked",
            Self::TurnControlWaitCancelled => "turn_control_wait_cancelled",
            Self::TurnControlWaitTimeout => "turn_control_wait_timeout",
            Self::TurnTerminalAwaitTimeout => "turn_terminal_await_timeout",
            Self::TurnTerminalDecode => "turn_terminal_decode",
            Self::TurnTerminalEncode => "turn_terminal_encode",
            Self::TurnTerminalInvalidResolution => "turn_terminal_invalid_resolution",
            Self::TurnTerminalUnknownOrRevoked => "turn_terminal_unknown_or_revoked",
            Self::ForeignCode(code) => code.as_str(),
        }
    }

    /// Whether retrying the identical operation is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::SessionExecutionBusy
                | Self::ManagedTurnConcurrencyLimitExceeded
                | Self::StoreCommitContended
                | Self::PostgresAwaitEventStore
                | Self::PostgresEffectJournalRetirement
                | Self::CancelStartGateUnavailable
                | Self::RestateAwaitEventAwait
                | Self::RestateAwaitEventCancel
                | Self::RestateAwaitEventPeek
                | Self::RestateAwaitEventResolve
                | Self::RestateAwaitEventRevocationRead
                | Self::RestateAwaitEventRevoke
                | Self::RestateAwaitEventSessionUpdate
                | Self::RestateTurnTerminalAttach
                | Self::RestateTurnTerminalAttachCeilingElapsed
                | Self::RuntimePerfStartGateRetry
                | Self::RuntimeStore
                | Self::SessionCommandPostDriveRefresh
                | Self::SessionCommandRefresh
                | Self::SessionCommandRefreshTools
                | Self::SqliteAwaitEventStore
                | Self::SqliteEffectJournalRetirement
                | Self::TransientCancelWatch
                | Self::TransientTerminalPublication
                | Self::TurnControlWaitTimeout
                | Self::TurnTerminalAwaitTimeout
        )
    }

    /// Whether retrying cannot succeed without changing input, configuration,
    /// wiring, or corrupted durable state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::MissingExecutionScopeId
                | Self::ExecutionScopeTurnIdMismatch
                | Self::StoreCommitNodeBudgetExceeded
                | Self::StoreCommitByteBudgetExceeded
                | Self::MissingProcessExecutionId
                | Self::DurableEffectLiveProtocolExtension
                | Self::DurableEffectLivePluginInput
                | Self::AwaitEventCancelUnsupported
                | Self::AwaitEventKeySign
                | Self::AwaitEventUnknownOrRevoked
                | Self::AwaitEventUnsupported
                | Self::EffectJournalRetirementUnsupported
                | Self::InvalidAwaitEventSessionId
                | Self::InvalidAwaitEventWaitIdentity
                | Self::InvalidTurnCancelRequest
                | Self::LlmProvider
                | Self::PostgresAwaitEventDecode
                | Self::PostgresAwaitEventEncode
                | Self::PostgresAwaitEventSign
                | Self::RestateEffectController
                | Self::RestateProcessTerminalEncode
                | Self::RestateSegmentProgramHashMismatch
                | Self::RestateTurnTerminalDecode
                | Self::RestateTurnTerminalInvalidResolution
                | Self::RuntimeStoreCorrupt
                | Self::SessionCommandClaim
                | Self::SessionCommandIdempotencyKey
                | Self::SessionDeleteScopeMismatch
                | Self::SessionToolRegistry
                | Self::SqliteAwaitEventDecode
                | Self::SqliteAwaitEventEncode
                | Self::SqliteAwaitEventSign
                | Self::ToolCompletionKeyMissingCallId
                | Self::ToolCompletionKeyProcessLifetime
                | Self::TurnCancelGateDecode
                | Self::TurnCancelGateEncode
                | Self::TurnCancelGateInvalidTerminal
                | Self::TurnCancellationEvidenceMissing
                | Self::TurnControlPeekOutcome
                | Self::TurnControlUnknownOrRevoked
                | Self::TurnTerminalDecode
                | Self::TurnTerminalEncode
                | Self::TurnTerminalInvalidResolution
                | Self::TurnTerminalUnknownOrRevoked
        )
    }

    /// Constructs a typed code from its stable wire representation.
    ///
    /// Built-in strings are always canonicalized to their dedicated variants;
    /// only unknown extension strings produce [`Self::ForeignCode`]. This is
    /// the supported construction path for host-defined codes.
    pub fn from_wire_code(code: &str) -> Self {
        match code {
            "missing_execution_scope_id" => Self::MissingExecutionScopeId,
            "execution_scope_turn_id_mismatch" => Self::ExecutionScopeTurnIdMismatch,
            "session_execution_busy" => Self::SessionExecutionBusy,
            "managed_turn_concurrency_limit_exceeded" => Self::ManagedTurnConcurrencyLimitExceeded,
            "session_execution_lease_lost" => Self::SessionExecutionLeaseLost,
            "store_commit_contended" => Self::StoreCommitContended,
            "store_commit_node_budget_exceeded" => Self::StoreCommitNodeBudgetExceeded,
            "store_commit_byte_budget_exceeded" => Self::StoreCommitByteBudgetExceeded,
            "missing_process_execution_id" => Self::MissingProcessExecutionId,
            "execution_state_capture_failed" => Self::ExecutionStateCaptureFailed,
            "resident_session_reload_failed" => Self::ResidentSessionReloadFailed,
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
            "await_event_cancel_unsupported" => Self::AwaitEventCancelUnsupported,
            "await_event_key_sign" => Self::AwaitEventKeySign,
            "await_event_unknown_or_revoked" => Self::AwaitEventUnknownOrRevoked,
            "await_event_unsupported" => Self::AwaitEventUnsupported,
            "cancel_start_gate_unavailable" => Self::CancelStartGateUnavailable,
            "effect_journal_retirement_unsupported" => Self::EffectJournalRetirementUnsupported,
            "invalid_await_event_session_id" => Self::InvalidAwaitEventSessionId,
            "invalid_await_event_wait_identity" => Self::InvalidAwaitEventWaitIdentity,
            "invalid_turn_cancel_request" => Self::InvalidTurnCancelRequest,
            "live_replay" => Self::LiveReplay,
            "llm_provider" => Self::LlmProvider,
            "postgres_await_event_decode" => Self::PostgresAwaitEventDecode,
            "postgres_await_event_encode" => Self::PostgresAwaitEventEncode,
            "postgres_await_event_notify" => Self::PostgresAwaitEventNotify,
            "postgres_await_event_sign" => Self::PostgresAwaitEventSign,
            "postgres_await_event_store" => Self::PostgresAwaitEventStore,
            "postgres_effect_journal_retirement" => Self::PostgresEffectJournalRetirement,
            "queued_work" => Self::QueuedWork,
            "restate_await_event_await" => Self::RestateAwaitEventAwait,
            "restate_await_event_cancel" => Self::RestateAwaitEventCancel,
            "restate_await_event_cancelled" => Self::RestateAwaitEventCancelled,
            "restate_await_event_peek" => Self::RestateAwaitEventPeek,
            "restate_await_event_resolve" => Self::RestateAwaitEventResolve,
            "restate_await_event_revocation_read" => Self::RestateAwaitEventRevocationRead,
            "restate_await_event_revoke" => Self::RestateAwaitEventRevoke,
            "restate_await_event_session_update" => Self::RestateAwaitEventSessionUpdate,
            "restate_effect_controller" => Self::RestateEffectController,
            "restate_process_terminal_encode" => Self::RestateProcessTerminalEncode,
            "restate_segment_program_hash_mismatch" => Self::RestateSegmentProgramHashMismatch,
            "restate_turn_terminal_attach" => Self::RestateTurnTerminalAttach,
            "restate_turn_terminal_attach_ceiling_elapsed" => {
                Self::RestateTurnTerminalAttachCeilingElapsed
            }
            "restate_turn_terminal_decode" => Self::RestateTurnTerminalDecode,
            "restate_turn_terminal_invalid_resolution" => {
                Self::RestateTurnTerminalInvalidResolution
            }
            "runtime_effect_controller_task_closed" => Self::RuntimeEffectControllerTaskClosed,
            "runtime_perf_start_gate_retry" => Self::RuntimePerfStartGateRetry,
            "runtime_store" => Self::RuntimeStore,
            "runtime_store_corrupt" => Self::RuntimeStoreCorrupt,
            "session_command_claim" => Self::SessionCommandClaim,
            "session_command_idempotency_key" => Self::SessionCommandIdempotencyKey,
            "session_command_post_drive_refresh" => Self::SessionCommandPostDriveRefresh,
            "session_command_refresh" => Self::SessionCommandRefresh,
            "session_command_refresh_tools" => Self::SessionCommandRefreshTools,
            "session_delete_scope_mismatch" => Self::SessionDeleteScopeMismatch,
            "session_head_refresh" => Self::SessionHeadRefresh,
            "session_tool_registry" => Self::SessionToolRegistry,
            "sqlite_await_event_decode" => Self::SqliteAwaitEventDecode,
            "sqlite_await_event_encode" => Self::SqliteAwaitEventEncode,
            "sqlite_await_event_notify" => Self::SqliteAwaitEventNotify,
            "sqlite_await_event_sign" => Self::SqliteAwaitEventSign,
            "sqlite_await_event_store" => Self::SqliteAwaitEventStore,
            "sqlite_effect_journal_retirement" => Self::SqliteEffectJournalRetirement,
            "tool_completion_key_missing_call_id" => Self::ToolCompletionKeyMissingCallId,
            "tool_completion_key_process_lifetime" => Self::ToolCompletionKeyProcessLifetime,
            "transient_cancel_watch" => Self::TransientCancelWatch,
            "transient_terminal_publication" => Self::TransientTerminalPublication,
            "turn_cancel_gate_decode" => Self::TurnCancelGateDecode,
            "turn_cancel_gate_encode" => Self::TurnCancelGateEncode,
            "turn_cancel_gate_invalid_terminal" => Self::TurnCancelGateInvalidTerminal,
            "turn_cancellation_evidence_missing" => Self::TurnCancellationEvidenceMissing,
            "turn_control_peek_outcome" => Self::TurnControlPeekOutcome,
            "turn_control_unknown_or_revoked" => Self::TurnControlUnknownOrRevoked,
            "turn_control_wait_cancelled" => Self::TurnControlWaitCancelled,
            "turn_control_wait_timeout" => Self::TurnControlWaitTimeout,
            "turn_terminal_await_timeout" => Self::TurnTerminalAwaitTimeout,
            "turn_terminal_decode" => Self::TurnTerminalDecode,
            "turn_terminal_encode" => Self::TurnTerminalEncode,
            "turn_terminal_invalid_resolution" => Self::TurnTerminalInvalidResolution,
            "turn_terminal_unknown_or_revoked" => Self::TurnTerminalUnknownOrRevoked,
            other => Self::ForeignCode(other.to_string()),
        }
    }
}

impl std::fmt::Display for RuntimeErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
        Ok(Self::from_wire_code(&code))
    }
}

/// Typed terminal cause retained when a controller-owned runtime effect must
/// abort through the generic runtime error boundary.
///
/// Every cause is terminal by construction. [`RuntimeError::is_terminal`]
/// therefore treats the presence of any cause as terminal, independently of
/// the code's ordinary classification.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeErrorCause {
    SessionDeleted { session_id: String },
}

/// Runtime error for unexpected failures.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeError {
    pub code: RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RuntimeErrorCause>,
}

impl RuntimeError {
    /// Constructs a `RuntimeError` for effect-host implementors while creating, observing, or
    /// resolving a durable wait.
    pub fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            cause: None,
        }
    }

    /// Sets the cause carried by a `RuntimeError` for effect-host implementors while creating,
    /// observing, or resolving a durable wait.
    pub fn with_cause(mut self, cause: RuntimeErrorCause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// Extracts the deleted session ID for effect-host implementors only from structured
    /// session-deletion causes, returning `None` for all other errors.
    pub fn deleted_session_id(&self) -> Option<&str> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::SessionDeleted { session_id } => Some(session_id),
        }
    }

    /// Whether retrying this exact failure is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        self.cause.is_none() && self.code.is_retryable()
    }

    /// Whether retrying cannot succeed without a host-side change.
    pub fn is_terminal(&self) -> bool {
        self.cause.is_some() || self.code.is_terminal()
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ExpectedClassification {
        Retryable,
        Terminal,
        Unknown,
    }

    fn expected_classification(code: &RuntimeErrorCode) -> ExpectedClassification {
        match code {
            RuntimeErrorCode::SessionExecutionBusy
            | RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded
            | RuntimeErrorCode::StoreCommitContended
            | RuntimeErrorCode::CancelStartGateUnavailable
            | RuntimeErrorCode::PostgresAwaitEventStore
            | RuntimeErrorCode::PostgresEffectJournalRetirement
            | RuntimeErrorCode::RestateAwaitEventAwait
            | RuntimeErrorCode::RestateAwaitEventCancel
            | RuntimeErrorCode::RestateAwaitEventPeek
            | RuntimeErrorCode::RestateAwaitEventResolve
            | RuntimeErrorCode::RestateAwaitEventRevocationRead
            | RuntimeErrorCode::RestateAwaitEventRevoke
            | RuntimeErrorCode::RestateAwaitEventSessionUpdate
            | RuntimeErrorCode::RestateTurnTerminalAttach
            | RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed
            | RuntimeErrorCode::RuntimePerfStartGateRetry
            | RuntimeErrorCode::RuntimeStore
            | RuntimeErrorCode::SessionCommandPostDriveRefresh
            | RuntimeErrorCode::SessionCommandRefresh
            | RuntimeErrorCode::SessionCommandRefreshTools
            | RuntimeErrorCode::SqliteAwaitEventStore
            | RuntimeErrorCode::SqliteEffectJournalRetirement
            | RuntimeErrorCode::TransientCancelWatch
            | RuntimeErrorCode::TransientTerminalPublication
            | RuntimeErrorCode::TurnControlWaitTimeout
            | RuntimeErrorCode::TurnTerminalAwaitTimeout => ExpectedClassification::Retryable,
            RuntimeErrorCode::MissingExecutionScopeId
            | RuntimeErrorCode::ExecutionScopeTurnIdMismatch
            | RuntimeErrorCode::StoreCommitNodeBudgetExceeded
            | RuntimeErrorCode::StoreCommitByteBudgetExceeded
            | RuntimeErrorCode::MissingProcessExecutionId
            | RuntimeErrorCode::DurableEffectLiveProtocolExtension
            | RuntimeErrorCode::DurableEffectLivePluginInput
            | RuntimeErrorCode::AwaitEventCancelUnsupported
            | RuntimeErrorCode::AwaitEventKeySign
            | RuntimeErrorCode::AwaitEventUnknownOrRevoked
            | RuntimeErrorCode::AwaitEventUnsupported
            | RuntimeErrorCode::EffectJournalRetirementUnsupported
            | RuntimeErrorCode::InvalidAwaitEventSessionId
            | RuntimeErrorCode::InvalidAwaitEventWaitIdentity
            | RuntimeErrorCode::InvalidTurnCancelRequest
            | RuntimeErrorCode::LlmProvider
            | RuntimeErrorCode::PostgresAwaitEventDecode
            | RuntimeErrorCode::PostgresAwaitEventEncode
            | RuntimeErrorCode::PostgresAwaitEventSign
            | RuntimeErrorCode::RestateEffectController
            | RuntimeErrorCode::RestateProcessTerminalEncode
            | RuntimeErrorCode::RestateSegmentProgramHashMismatch
            | RuntimeErrorCode::RestateTurnTerminalDecode
            | RuntimeErrorCode::RestateTurnTerminalInvalidResolution
            | RuntimeErrorCode::RuntimeStoreCorrupt
            | RuntimeErrorCode::SessionCommandClaim
            | RuntimeErrorCode::SessionCommandIdempotencyKey
            | RuntimeErrorCode::SessionDeleteScopeMismatch
            | RuntimeErrorCode::SessionToolRegistry
            | RuntimeErrorCode::SqliteAwaitEventDecode
            | RuntimeErrorCode::SqliteAwaitEventEncode
            | RuntimeErrorCode::SqliteAwaitEventSign
            | RuntimeErrorCode::ToolCompletionKeyMissingCallId
            | RuntimeErrorCode::ToolCompletionKeyProcessLifetime
            | RuntimeErrorCode::TurnCancelGateDecode
            | RuntimeErrorCode::TurnCancelGateEncode
            | RuntimeErrorCode::TurnCancelGateInvalidTerminal
            | RuntimeErrorCode::TurnCancellationEvidenceMissing
            | RuntimeErrorCode::TurnControlPeekOutcome
            | RuntimeErrorCode::TurnControlUnknownOrRevoked
            | RuntimeErrorCode::TurnTerminalDecode
            | RuntimeErrorCode::TurnTerminalEncode
            | RuntimeErrorCode::TurnTerminalInvalidResolution
            | RuntimeErrorCode::TurnTerminalUnknownOrRevoked => ExpectedClassification::Terminal,
            RuntimeErrorCode::SessionExecutionLeaseLost
            | RuntimeErrorCode::ExecutionStateCaptureFailed
            | RuntimeErrorCode::ResidentSessionReloadFailed
            | RuntimeErrorCode::StoreCommitFailed
            | RuntimeErrorCode::PluginSessionManager
            | RuntimeErrorCode::PluginFinalizeTurn
            | RuntimeErrorCode::PluginCheckpoint
            | RuntimeErrorCode::PluginPrepareTurn
            | RuntimeErrorCode::ContextPrepareTurn
            | RuntimeErrorCode::ProtocolTurnExtension
            | RuntimeErrorCode::ProtocolBeforeLlmCall
            | RuntimeErrorCode::TurnStreamJoin
            | RuntimeErrorCode::EmptyAgentFrameRun
            | RuntimeErrorCode::LiveReplay
            | RuntimeErrorCode::PostgresAwaitEventNotify
            | RuntimeErrorCode::QueuedWork
            | RuntimeErrorCode::RestateAwaitEventCancelled
            | RuntimeErrorCode::RuntimeEffectControllerTaskClosed
            | RuntimeErrorCode::SessionHeadRefresh
            | RuntimeErrorCode::SqliteAwaitEventNotify
            | RuntimeErrorCode::TurnControlWaitCancelled
            | RuntimeErrorCode::ForeignCode(_) => ExpectedClassification::Unknown,
        }
    }

    #[test]
    fn runtime_error_code_classification_is_exhaustive_and_disjoint() {
        let codes = [
            RuntimeErrorCode::MissingExecutionScopeId,
            RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
            RuntimeErrorCode::SessionExecutionBusy,
            RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded,
            RuntimeErrorCode::SessionExecutionLeaseLost,
            RuntimeErrorCode::StoreCommitContended,
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            RuntimeErrorCode::StoreCommitByteBudgetExceeded,
            RuntimeErrorCode::MissingProcessExecutionId,
            RuntimeErrorCode::ExecutionStateCaptureFailed,
            RuntimeErrorCode::ResidentSessionReloadFailed,
            RuntimeErrorCode::StoreCommitFailed,
            RuntimeErrorCode::PluginSessionManager,
            RuntimeErrorCode::PluginFinalizeTurn,
            RuntimeErrorCode::PluginCheckpoint,
            RuntimeErrorCode::PluginPrepareTurn,
            RuntimeErrorCode::ContextPrepareTurn,
            RuntimeErrorCode::ProtocolTurnExtension,
            RuntimeErrorCode::ProtocolBeforeLlmCall,
            RuntimeErrorCode::TurnStreamJoin,
            RuntimeErrorCode::EmptyAgentFrameRun,
            RuntimeErrorCode::DurableEffectLiveProtocolExtension,
            RuntimeErrorCode::DurableEffectLivePluginInput,
            RuntimeErrorCode::AwaitEventCancelUnsupported,
            RuntimeErrorCode::AwaitEventKeySign,
            RuntimeErrorCode::AwaitEventUnknownOrRevoked,
            RuntimeErrorCode::AwaitEventUnsupported,
            RuntimeErrorCode::CancelStartGateUnavailable,
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            RuntimeErrorCode::InvalidAwaitEventSessionId,
            RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
            RuntimeErrorCode::InvalidTurnCancelRequest,
            RuntimeErrorCode::LiveReplay,
            RuntimeErrorCode::LlmProvider,
            RuntimeErrorCode::PostgresAwaitEventDecode,
            RuntimeErrorCode::PostgresAwaitEventEncode,
            RuntimeErrorCode::PostgresAwaitEventNotify,
            RuntimeErrorCode::PostgresAwaitEventSign,
            RuntimeErrorCode::PostgresAwaitEventStore,
            RuntimeErrorCode::PostgresEffectJournalRetirement,
            RuntimeErrorCode::QueuedWork,
            RuntimeErrorCode::RestateAwaitEventAwait,
            RuntimeErrorCode::RestateAwaitEventCancel,
            RuntimeErrorCode::RestateAwaitEventCancelled,
            RuntimeErrorCode::RestateAwaitEventPeek,
            RuntimeErrorCode::RestateAwaitEventResolve,
            RuntimeErrorCode::RestateAwaitEventRevocationRead,
            RuntimeErrorCode::RestateAwaitEventRevoke,
            RuntimeErrorCode::RestateAwaitEventSessionUpdate,
            RuntimeErrorCode::RestateEffectController,
            RuntimeErrorCode::RestateProcessTerminalEncode,
            RuntimeErrorCode::RestateSegmentProgramHashMismatch,
            RuntimeErrorCode::RestateTurnTerminalAttach,
            RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed,
            RuntimeErrorCode::RestateTurnTerminalDecode,
            RuntimeErrorCode::RestateTurnTerminalInvalidResolution,
            RuntimeErrorCode::RuntimeEffectControllerTaskClosed,
            RuntimeErrorCode::RuntimePerfStartGateRetry,
            RuntimeErrorCode::RuntimeStore,
            RuntimeErrorCode::RuntimeStoreCorrupt,
            RuntimeErrorCode::SessionCommandClaim,
            RuntimeErrorCode::SessionCommandIdempotencyKey,
            RuntimeErrorCode::SessionCommandPostDriveRefresh,
            RuntimeErrorCode::SessionCommandRefresh,
            RuntimeErrorCode::SessionCommandRefreshTools,
            RuntimeErrorCode::SessionDeleteScopeMismatch,
            RuntimeErrorCode::SessionHeadRefresh,
            RuntimeErrorCode::SessionToolRegistry,
            RuntimeErrorCode::SqliteAwaitEventDecode,
            RuntimeErrorCode::SqliteAwaitEventEncode,
            RuntimeErrorCode::SqliteAwaitEventNotify,
            RuntimeErrorCode::SqliteAwaitEventSign,
            RuntimeErrorCode::SqliteAwaitEventStore,
            RuntimeErrorCode::SqliteEffectJournalRetirement,
            RuntimeErrorCode::ToolCompletionKeyMissingCallId,
            RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
            RuntimeErrorCode::TransientCancelWatch,
            RuntimeErrorCode::TransientTerminalPublication,
            RuntimeErrorCode::TurnCancelGateDecode,
            RuntimeErrorCode::TurnCancelGateEncode,
            RuntimeErrorCode::TurnCancelGateInvalidTerminal,
            RuntimeErrorCode::TurnCancellationEvidenceMissing,
            RuntimeErrorCode::TurnControlPeekOutcome,
            RuntimeErrorCode::TurnControlUnknownOrRevoked,
            RuntimeErrorCode::TurnControlWaitCancelled,
            RuntimeErrorCode::TurnControlWaitTimeout,
            RuntimeErrorCode::TurnTerminalAwaitTimeout,
            RuntimeErrorCode::TurnTerminalDecode,
            RuntimeErrorCode::TurnTerminalEncode,
            RuntimeErrorCode::TurnTerminalInvalidResolution,
            RuntimeErrorCode::TurnTerminalUnknownOrRevoked,
            RuntimeErrorCode::from_wire_code("plugin_defined_abort"),
        ];

        for code in codes {
            let actual = match (code.is_retryable(), code.is_terminal()) {
                (true, false) => ExpectedClassification::Retryable,
                (false, true) => ExpectedClassification::Terminal,
                (false, false) => ExpectedClassification::Unknown,
                (true, true) => panic!("{} is both retryable and terminal", code.as_str()),
            };
            assert_eq!(actual, expected_classification(&code), "{code}");

            let json = serde_json::to_value(&code).expect("serialize typed code");
            let decoded: RuntimeErrorCode =
                serde_json::from_value(json).expect("deserialize typed code");
            assert_eq!(decoded, code, "typed round trip for {code}");
        }
    }

    #[test]
    fn terminal_cause_overrides_retryable_runtime_store_code() {
        let error = RuntimeError::new(RuntimeErrorCode::RuntimeStore, "session deleted")
            .with_cause(super::RuntimeErrorCause::SessionDeleted {
                session_id: "retired".to_string(),
            });

        assert!(!error.is_retryable());
        assert!(error.is_terminal());
    }

    #[test]
    fn foreign_runtime_error_code_round_trips() {
        let decoded: RuntimeError = serde_json::from_value(serde_json::json!({
            "code": "plugin_defined_abort",
            "message": "stopped by plugin"
        }))
        .expect("decode plugin runtime error");

        assert_eq!(
            decoded.code,
            RuntimeErrorCode::from_wire_code("plugin_defined_abort")
        );
        assert_eq!(decoded.code.as_str(), "plugin_defined_abort");
    }

    #[test]
    fn wire_constructor_canonicalizes_built_in_codes() {
        let code = RuntimeErrorCode::from_wire_code("runtime_store");

        assert_eq!(code, RuntimeErrorCode::RuntimeStore);
        assert!(code.is_retryable());
        assert!(!code.is_terminal());
    }
}
