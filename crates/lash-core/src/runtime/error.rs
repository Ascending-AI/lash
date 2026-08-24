use crate::SessionError;

/// Stable runtime error code.
///
/// Codes serialize as the same snake_case strings exposed in traces and host
/// errors, but callers should match this type instead of parsing display text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuntimeErrorCode {
    AttachmentSourcePolicyDenied,
    EffectPanicked,
    MissingExecutionScopeId,
    ExecutionScopeTurnIdMismatch,
    /// The managed-turn registry's admission cap is full. Retrying the
    /// same request after another managed turn finishes is safe.
    ManagedTurnConcurrencyLimitExceeded,
    SessionExecutionLeaseLost,
    /// A durable workflow controller's queued-work drain could not take the
    /// session execution lane: a live foreign executor holds it. Retrying the
    /// identical drain is explicitly safe, and pacing belongs to the engine's
    /// retry policy - the runtime deliberately stops waiting instead of
    /// blocking one invocation indefinitely.
    SessionExecutionLaneBusy,
    /// A turn that drove the acceptance it minted, without a claim on it, lost
    /// the head CAS to whoever holds or already settled that row (ADR 0069
    /// §5). The drive attempt is retired as superseded: no durable record was
    /// written, the settlement is never retried under a new authority, and the
    /// row stays exactly where recovery expects to find it. Re-running the
    /// identical turn is explicitly safe and is how the result is obtained -
    /// the journaled acceptance re-derives the same admission, so a re-run
    /// either drives the row or finds it settled and replays the original
    /// commit's receipt rather than duplicating it (ADR 0069 §6).
    TurnInputSettlementSuperseded,
    /// The store aborted a commit before publication because transactional
    /// write authority was contended. Retrying the same operation unchanged is
    /// safe; reloading or rebasing is not required.
    StoreCommitContended,
    /// The final runtime commit writes more graph and attachment-adoption rows
    /// than the shared node budget permits. The same turn will fail identically
    /// until the host produces a smaller turn.
    StoreCommitNodeBudgetExceeded,
    /// The final runtime commit contains more persisted payload bytes than the
    /// shared transaction budget permits. The same turn will fail identically
    /// until the host produces a smaller turn.
    StoreCommitByteBudgetExceeded,
    /// A checkpoint component uses a codec version this build cannot read or
    /// write. The same commit cannot succeed until the store/session is
    /// recreated with a compatible Lash version.
    CheckpointComponentEncodingVersionMismatch,
    /// A durable record failed deterministic serialization before publication.
    /// Retrying the same value with the same build cannot change the result.
    RecordEncodingFailed,
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
    EffectGroupUnsupported,
    EffectJournalRetirementUnsupported,
    InvalidAwaitEventSessionId,
    InvalidAwaitEventWaitIdentity,
    InvalidTurnCancelRequest,
    LiveReplay,
    LlmProvider,
    Plugin,
    PostgresEffectReplayCorruptRow,
    PostgresEffectReplayDecode,
    PostgresEffectReplayEncode,
    PostgresEffectReplayHashConflict,
    PostgresEffectReplayKeyMissing,
    PostgresEffectReplayLeaseLost,
    PostgresEffectReplayMissing,
    PostgresEffectReplayStore,
    PostgresAwaitEventDecode,
    PostgresAwaitEventEncode,
    /// Process-local; repaired by restart, not by same-process retry.
    PostgresAwaitEventNotify,
    PostgresAwaitEventSign,
    PostgresAwaitEventStore,
    PostgresEffectJournalRetirement,
    QueuedWork,
    /// One queued row alone renders larger than the whole model context window,
    /// so no drain policy can make it fit and an automatic drain cannot execute
    /// it (FIG-1313). Retrying the identical drain fails identically until the
    /// row is cancelled or the window grows.
    QueuedWorkRowExceedsContextWindow,
    ProcessPanicked,
    /// ADR 0051 effect-host implementor diagnostic for a process-command
    /// refusal whose target is outside the invoking session's visible set.
    ProcessNotVisible,
    /// ADR 0051 effect-host implementor diagnostic for a write or cancellation
    /// refused because the recorded target is already terminal.
    ProcessAlreadyTerminal,
    /// ADR 0051 effect-host implementor diagnostic for a process-command
    /// refusal whose terminal target has been replaced by a retention tombstone.
    ProcessNoLongerRetained,
    ProcessRegistryUnavailable,
    ProcessSignalWaitCancelled,
    ProcessSignalWaitTimeout,
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
    RestateEffectHashMismatch,
    RestateEffectHostRequiresHandlerScope,
    /// A journaled Restate effect produced an outcome the durable journal can
    /// never accept, so the effect gave up with a terminal failure instead of
    /// failing every redrive of the enclosing turn.
    RestateJournaledEffectPoisoned,
    RestateProcessAwait,
    RestateProcessAwaitAfterTurnCancel,
    RestateProcessTurnCancelContextMissing,
    RestateProcessTerminalEncode,
    RestateTurnTerminalAttach,
    /// A bounded Restate terminal attachment elapsed while the durable wait
    /// remained live. Re-attaching the same address is explicitly safe.
    RestateTurnTerminalAttachCeilingElapsed,
    RestateTurnTerminalDecode,
    RestateTurnTerminalInvalidResolution,
    RestateTurnCancelScopeMismatch,
    RestateTurnCancelScopeMissing,
    /// A host assistant-response hook failed while deriving the transformed
    /// response from an already-journaled raw provider completion (FIG-1276).
    ///
    /// Deliberately **not** terminal: the paid completion is durable in phase
    /// 1, so the correct recovery is to redrive phase 2 and derive again rather
    /// than to seal an incomplete derivation into the journal.
    RuntimeEffectAssistantResponseHook,
    RuntimeEffectAttachmentStore,
    RuntimeEffectEnvelopeCanonicalDecode,
    RuntimeEffectEnvelopeCanonicalHashInvariant,
    RuntimeEffectEnvelopeHash,
    /// A grouped settlement await was cancelled by its cancellation token. The
    /// group's durable rank is untouched, so a later await resumes at the same
    /// rank.
    RuntimeEffectGroupAwaitCancelled,
    /// A durable effect group's child was cancelled because the group's loser
    /// disposition resolved to `Cancel`. The cancellation is that child's
    /// terminal, not a transient failure to retry.
    RuntimeEffectGroupChildCancelled,
    /// A drain pass was not attempted because the process asked to run it is
    /// still working the group itself — a caller here has it open, or children
    /// this host dispatched have not settled yet.
    ///
    /// Retryable, and the distinction matters: nothing about the group is wrong
    /// and nothing needs changing, so the same call succeeds once this host is
    /// done with it. A refusal that meant "never" would carry
    /// `RuntimeEffectGroupShape` instead.
    RuntimeEffectGroupDrainDeferred,
    /// A durable effect group was assembled with children that disagree with the
    /// group they claim to belong to, or an effect carrying group membership
    /// reached a command shape that cannot honor it.
    RuntimeEffectGroupShape,
    RuntimeEffectInvocationKind,
    RuntimeEffectInvocationSubject,
    RuntimeEffectLocalExecutorMismatch,
    RuntimeEffectLocalExecutorUnavailable,
    RuntimeEffectLocalTaskClosed,
    RuntimeEffectProcessTaskJoin,
    RuntimeEffectReplayRequired,
    RuntimeEffectSleepCancelled,
    RuntimeEffectTaskJoin,
    RuntimeEffectToolAttemptCallId,
    RuntimeEffectToolAttemptIndex,
    RuntimeEffectToolBatchCallId,
    RuntimeEffectToolBatchCallReplay,
    RuntimeEffectToolBatchEmpty,
    RuntimeEffectToolBatchId,
    RuntimeEffectWrongOutcome,
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
    SqliteEffectReplayCorruptRow,
    SqliteEffectReplayDecode,
    SqliteEffectReplayEncode,
    SqliteEffectReplayHashConflict,
    SqliteEffectReplayKeyMissing,
    SqliteEffectReplayLeaseLost,
    SqliteEffectReplayMissing,
    SqliteEffectReplayStore,
    ToolBatchMissingResult,
    ToolBatchResultCountMismatch,
    ToolCatalogResolutionFailed,
    ToolCompletionKeyMissingCallId,
    ToolCompletionKeyProcessLifetime,
    ToolDeferralNotDeclared,
    TransientCancelWatch,
    TransientTerminalPublication,
    TurnCancelGateDecode,
    TurnCancelGateEncode,
    TurnCancelGateInvalidTerminal,
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
    TriggerStoreUnavailable,
    /// A code minted by a public plugin or effect-host extension point.
    ///
    /// Built-in `RuntimeError` constructors use typed variants; open plugin and
    /// effect-controller boundaries use this for host-defined or controller-local
    /// codes. Extensions must namespace codes and avoid built-in `as_str` values.
    /// Foreign codes are conservatively neither retryable nor terminal.
    #[non_exhaustive]
    ForeignCode(String),
}

pub(super) fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    match err {
        crate::store::StoreError::Contended => RuntimeError::new(
            RuntimeErrorCode::StoreCommitContended,
            "store commit is contended; retry the identical operation unchanged",
        ),
        err @ crate::store::StoreError::CommitNodeBudgetExceeded { .. } => RuntimeError::new(
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            err.to_string(),
        ),
        err @ crate::store::StoreError::CommitByteBudgetExceeded { .. } => RuntimeError::new(
            RuntimeErrorCode::StoreCommitByteBudgetExceeded,
            err.to_string(),
        ),
        err @ crate::store::StoreError::CheckpointComponentEncodingVersionMismatch { .. } => {
            RuntimeError::new(
                RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch,
                err.to_string(),
            )
        }
        err @ crate::store::StoreError::RecordEncodingFailed { .. } => {
            RuntimeError::new(RuntimeErrorCode::RecordEncodingFailed, err.to_string())
        }
        // ADR 0069 §5(d): a driver that settled the row it accepted without a
        // claim, and found that row held or already settled, ceded at the head
        // CAS. Nothing durable was written: a stand-down, not a commit fault.
        err @ crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded { .. } => {
            RuntimeError::new(
                RuntimeErrorCode::TurnInputSettlementSuperseded,
                err.to_string(),
            )
        }
        crate::store::StoreError::SessionExecutionLeaseExpired { session_id } => RuntimeError::new(
            RuntimeErrorCode::SessionExecutionLeaseLost,
            format!("session execution lease for session `{session_id}` was lost before commit"),
        ),
        crate::store::StoreError::ExecutionStateCaptureFailed { message } => RuntimeError::new(
            RuntimeErrorCode::ExecutionStateCaptureFailed,
            format!("failed to snapshot dirty execution state: {message}"),
        ),
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
}

/// Wrap a store commit failure for the session-facing API.
///
/// The typed arm is not a convenience list: every variant here is one a host is
/// expected to *match on* rather than log. `HeadRevisionConflict` in particular
/// is the concurrent-append outcome the host is told to refresh and retry from,
/// so collapsing it into `Protocol(String)` would leave string matching as the
/// only way to tell a lost head race from an unrelated store failure.
pub(super) fn session_commit_error(
    context: &str,
    source: crate::store::StoreError,
) -> SessionError {
    match source {
        source @ (crate::store::StoreError::SessionDeleted { .. }
        | crate::store::StoreError::SessionStateVersionNewerThanRuntime { .. }
        | crate::store::StoreError::SessionStateMigrationUnavailable { .. }
        | crate::store::StoreError::HeadRevisionConflict { .. }
        | crate::store::StoreError::AppendOperationIdentityConflict { .. }
        | crate::store::StoreError::AppendReceiptRequestedNodeCountCorrupt { .. }
        | crate::store::StoreError::CommitNodeBudgetExceeded { .. }
        | crate::store::StoreError::CommitByteBudgetExceeded { .. }
        | crate::store::StoreError::CheckpointComponentEncodingVersionMismatch {
            ..
        }
        | crate::store::StoreError::RecordEncodingFailed { .. }) => SessionError::Store {
            context: context.to_string(),
            source,
        },
        source => SessionError::Protocol(format!("{context}: {source}")),
    }
}

#[cfg(test)]
mod store_commit_error_tests {
    use super::{RuntimeErrorCode, runtime_error_from_store_commit};
    use crate::store::StoreError;

    #[test]
    fn commit_budget_errors_preserve_the_budget_kind_and_limits() {
        let node_error = runtime_error_from_store_commit(StoreError::CommitNodeBudgetExceeded {
            node_count: 513,
            max_nodes: 512,
        });
        assert_eq!(
            node_error.code,
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded
        );
        assert!(
            node_error
                .message
                .contains("records 513 rows for this attempt")
        );
        assert!(
            node_error
                .message
                .contains("configured 512-row node budget")
        );
        assert!(
            node_error
                .message
                .contains("including attachment-intent adoption")
        );

        let byte_error = runtime_error_from_store_commit(StoreError::CommitByteBudgetExceeded {
            session_config_bytes: 0,
            graph_delta_bytes: 900_000,
            checkpoint_bytes: 150_000,
            attachment_manifest_bytes: 1,
            total_bytes: 1_050_001,
            max_bytes: 1_048_576,
        });
        assert_eq!(
            byte_error.code,
            RuntimeErrorCode::StoreCommitByteBudgetExceeded
        );
        assert!(
            byte_error
                .message
                .contains("1050001 budgeted payload bytes")
        );
        assert!(
            byte_error
                .message
                .contains("1048576-byte transaction budget")
        );
    }

    #[test]
    fn deterministic_checkpoint_commit_errors_are_typed_and_terminal() {
        let mismatch = runtime_error_from_store_commit(
            StoreError::CheckpointComponentEncodingVersionMismatch {
                key: "execution_state".to_string(),
                actual: 2,
                expected: 1,
            },
        );
        assert_eq!(
            mismatch.code,
            RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch
        );
        assert!(mismatch.code.is_terminal());
        assert!(!mismatch.code.is_retryable());
        assert!(mismatch.message.contains("execution_state"));

        let encoding = runtime_error_from_store_commit(StoreError::RecordEncodingFailed {
            record_kind: "checkpoint root".to_string(),
            message: "deterministic fixture failure".to_string(),
        });
        assert_eq!(encoding.code, RuntimeErrorCode::RecordEncodingFailed);
        assert!(encoding.code.is_terminal());
        assert!(!encoding.code.is_retryable());
        assert!(encoding.message.contains("checkpoint root"));
    }

    #[test]
    fn public_append_and_park_preserve_deterministic_store_errors() {
        for error in [
            StoreError::CheckpointComponentEncodingVersionMismatch {
                key: "execution_state".to_string(),
                actual: 2,
                expected: 1,
            },
            StoreError::RecordEncodingFailed {
                record_kind: "checkpoint root".to_string(),
                message: "deterministic fixture failure".to_string(),
            },
        ] {
            let expected_variant = error.variant_name();
            let session_error =
                super::session_commit_error("public append and park persistence boundary", error);
            assert!(
                matches!(
                    session_error,
                    crate::SessionError::Store { ref source, .. }
                        if source.variant_name() == expected_variant
                ),
                "{expected_variant} lost its typed store identity: {session_error}"
            );
        }
    }
}

impl RuntimeErrorCode {
    /// Provides the canonical str view to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn as_str(&self) -> &str {
        match self {
            Self::AttachmentSourcePolicyDenied => "attachment_source_policy_denied",
            Self::EffectPanicked => "effect_panicked",
            Self::MissingExecutionScopeId => "missing_execution_scope_id",
            Self::ExecutionScopeTurnIdMismatch => "execution_scope_turn_id_mismatch",
            Self::ManagedTurnConcurrencyLimitExceeded => "managed_turn_concurrency_limit_exceeded",
            Self::SessionExecutionLeaseLost => "session_execution_lease_lost",
            Self::SessionExecutionLaneBusy => "session_execution_lane_busy",
            Self::TurnInputSettlementSuperseded => "turn_input_settlement_superseded",
            Self::StoreCommitContended => "store_commit_contended",
            Self::StoreCommitNodeBudgetExceeded => "store_commit_node_budget_exceeded",
            Self::StoreCommitByteBudgetExceeded => "store_commit_byte_budget_exceeded",
            Self::CheckpointComponentEncodingVersionMismatch => {
                "checkpoint_component_encoding_version_mismatch"
            }
            Self::RecordEncodingFailed => "record_encoding_failed",
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
            Self::EffectGroupUnsupported => "effect_group_unsupported",
            Self::EffectJournalRetirementUnsupported => "effect_journal_retirement_unsupported",
            Self::InvalidAwaitEventSessionId => "invalid_await_event_session_id",
            Self::InvalidAwaitEventWaitIdentity => "invalid_await_event_wait_identity",
            Self::InvalidTurnCancelRequest => "invalid_turn_cancel_request",
            Self::LiveReplay => "live_replay",
            Self::LlmProvider => "llm_provider",
            Self::Plugin => "plugin",
            Self::PostgresEffectReplayCorruptRow => "postgres_effect_replay_corrupt_row",
            Self::PostgresEffectReplayDecode => "postgres_effect_replay_decode",
            Self::PostgresEffectReplayEncode => "postgres_effect_replay_encode",
            Self::PostgresEffectReplayHashConflict => "postgres_effect_replay_hash_conflict",
            Self::PostgresEffectReplayKeyMissing => "postgres_effect_replay_key_missing",
            Self::PostgresEffectReplayLeaseLost => "postgres_effect_replay_lease_lost",
            Self::PostgresEffectReplayMissing => "postgres_effect_replay_missing",
            Self::PostgresEffectReplayStore => "postgres_effect_replay_store",
            Self::PostgresAwaitEventDecode => "postgres_await_event_decode",
            Self::PostgresAwaitEventEncode => "postgres_await_event_encode",
            Self::PostgresAwaitEventNotify => "postgres_await_event_notify",
            Self::PostgresAwaitEventSign => "postgres_await_event_sign",
            Self::PostgresAwaitEventStore => "postgres_await_event_store",
            Self::PostgresEffectJournalRetirement => "postgres_effect_journal_retirement",
            Self::QueuedWork => "queued_work",
            Self::QueuedWorkRowExceedsContextWindow => "queued_work_row_exceeds_context_window",
            Self::ProcessPanicked => "process_panicked",
            Self::ProcessNotVisible => "process_not_visible",
            Self::ProcessAlreadyTerminal => "process_already_terminal",
            Self::ProcessNoLongerRetained => "process_no_longer_retained",
            Self::ProcessRegistryUnavailable => "process_registry_unavailable",
            Self::ProcessSignalWaitCancelled => "process_signal_wait_cancelled",
            Self::ProcessSignalWaitTimeout => "process_signal_wait_timeout",
            Self::RestateAwaitEventAwait => "restate_await_event_await",
            Self::RestateAwaitEventCancel => "restate_await_event_cancel",
            Self::RestateAwaitEventCancelled => "restate_await_event_cancelled",
            Self::RestateAwaitEventPeek => "restate_await_event_peek",
            Self::RestateAwaitEventResolve => "restate_await_event_resolve",
            Self::RestateAwaitEventRevocationRead => "restate_await_event_revocation_read",
            Self::RestateAwaitEventRevoke => "restate_await_event_revoke",
            Self::RestateAwaitEventSessionUpdate => "restate_await_event_session_update",
            Self::RestateEffectController => "restate_effect_controller",
            Self::RestateEffectHashMismatch => "restate_effect_hash_mismatch",
            Self::RestateJournaledEffectPoisoned => "restate_journaled_effect_poisoned",
            Self::RestateEffectHostRequiresHandlerScope => {
                "restate_effect_host_requires_handler_scope"
            }
            Self::RestateProcessAwait => "restate_process_await",
            Self::RestateProcessAwaitAfterTurnCancel => "restate_process_await_after_turn_cancel",
            Self::RestateProcessTurnCancelContextMissing => {
                "restate_process_turn_cancel_context_missing"
            }
            Self::RestateProcessTerminalEncode => "restate_process_terminal_encode",
            Self::RestateTurnTerminalAttach => "restate_turn_terminal_attach",
            Self::RestateTurnTerminalAttachCeilingElapsed => {
                "restate_turn_terminal_attach_ceiling_elapsed"
            }
            Self::RestateTurnTerminalDecode => "restate_turn_terminal_decode",
            Self::RestateTurnTerminalInvalidResolution => {
                "restate_turn_terminal_invalid_resolution"
            }
            Self::RestateTurnCancelScopeMismatch => "restate_turn_cancel_scope_mismatch",
            Self::RestateTurnCancelScopeMissing => "restate_turn_cancel_scope_missing",
            Self::RuntimeEffectAttachmentStore => "runtime_effect_attachment_store",
            Self::RuntimeEffectAssistantResponseHook => "runtime_effect_assistant_response_hook",
            Self::RuntimeEffectEnvelopeCanonicalDecode => {
                "runtime_effect_envelope_canonical_decode"
            }
            Self::RuntimeEffectEnvelopeCanonicalHashInvariant => {
                "runtime_effect_envelope_canonical_hash_invariant"
            }
            Self::RuntimeEffectEnvelopeHash => "runtime_effect_envelope_hash",
            Self::RuntimeEffectGroupAwaitCancelled => "runtime_effect_group_await_cancelled",
            Self::RuntimeEffectGroupChildCancelled => "runtime_effect_group_child_cancelled",
            Self::RuntimeEffectGroupDrainDeferred => "runtime_effect_group_drain_deferred",
            Self::RuntimeEffectGroupShape => "runtime_effect_group_shape",
            Self::RuntimeEffectInvocationKind => "runtime_effect_invocation_kind",
            Self::RuntimeEffectInvocationSubject => "runtime_effect_invocation_subject",
            Self::RuntimeEffectLocalExecutorMismatch => "runtime_effect_local_executor_mismatch",
            Self::RuntimeEffectLocalExecutorUnavailable => {
                "runtime_effect_local_executor_unavailable"
            }
            Self::RuntimeEffectLocalTaskClosed => "runtime_effect_local_task_closed",
            Self::RuntimeEffectProcessTaskJoin => "runtime_effect_process_task_join",
            Self::RuntimeEffectReplayRequired => "runtime_effect_replay_required",
            Self::RuntimeEffectSleepCancelled => "runtime_effect_sleep_cancelled",
            Self::RuntimeEffectTaskJoin => "runtime_effect_task_join",
            Self::RuntimeEffectToolAttemptCallId => "runtime_effect_tool_attempt_call_id",
            Self::RuntimeEffectToolAttemptIndex => "runtime_effect_tool_attempt_index",
            Self::RuntimeEffectToolBatchCallId => "runtime_effect_tool_batch_call_id",
            Self::RuntimeEffectToolBatchCallReplay => "runtime_effect_tool_batch_call_replay",
            Self::RuntimeEffectToolBatchEmpty => "runtime_effect_tool_batch_empty",
            Self::RuntimeEffectToolBatchId => "runtime_effect_tool_batch_id",
            Self::RuntimeEffectWrongOutcome => "runtime_effect_wrong_outcome",
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
            Self::SqliteEffectReplayCorruptRow => "sqlite_effect_replay_corrupt_row",
            Self::SqliteEffectReplayDecode => "sqlite_effect_replay_decode",
            Self::SqliteEffectReplayEncode => "sqlite_effect_replay_encode",
            Self::SqliteEffectReplayHashConflict => "sqlite_effect_replay_hash_conflict",
            Self::SqliteEffectReplayKeyMissing => "sqlite_effect_replay_key_missing",
            Self::SqliteEffectReplayLeaseLost => "sqlite_effect_replay_lease_lost",
            Self::SqliteEffectReplayMissing => "sqlite_effect_replay_missing",
            Self::SqliteEffectReplayStore => "sqlite_effect_replay_store",
            Self::ToolBatchMissingResult => "tool_batch_missing_result",
            Self::ToolBatchResultCountMismatch => "tool_batch_result_count_mismatch",
            Self::ToolCatalogResolutionFailed => "tool_catalog_resolution_failed",
            Self::ToolCompletionKeyMissingCallId => "tool_completion_key_missing_call_id",
            Self::ToolCompletionKeyProcessLifetime => "tool_completion_key_process_lifetime",
            Self::ToolDeferralNotDeclared => "tool_deferral_not_declared",
            Self::TransientCancelWatch => "transient_cancel_watch",
            Self::TransientTerminalPublication => "transient_terminal_publication",
            Self::TurnCancelGateDecode => "turn_cancel_gate_decode",
            Self::TurnCancelGateEncode => "turn_cancel_gate_encode",
            Self::TurnCancelGateInvalidTerminal => "turn_cancel_gate_invalid_terminal",
            Self::TurnControlPeekOutcome => "turn_control_peek_outcome",
            Self::TurnControlUnknownOrRevoked => "turn_control_unknown_or_revoked",
            Self::TurnControlWaitCancelled => "turn_control_wait_cancelled",
            Self::TurnControlWaitTimeout => "turn_control_wait_timeout",
            Self::TurnTerminalAwaitTimeout => "turn_terminal_await_timeout",
            Self::TurnTerminalDecode => "turn_terminal_decode",
            Self::TurnTerminalEncode => "turn_terminal_encode",
            Self::TurnTerminalInvalidResolution => "turn_terminal_invalid_resolution",
            Self::TurnTerminalUnknownOrRevoked => "turn_terminal_unknown_or_revoked",
            Self::TriggerStoreUnavailable => "trigger_store_unavailable",
            Self::ForeignCode(code) => code.as_str(),
        }
    }

    /// Whether this code reports that a replayed runtime effect diverged from
    /// the effect envelope recorded by its durable controller.
    ///
    /// The store-qualified wire codes remain available for display and
    /// diagnostics. Hosts should use this predicate instead of matching those
    /// backend-specific strings when choosing alerting or drain policy.
    pub fn is_replay_mismatch(&self) -> bool {
        matches!(
            self.as_str(),
            "sqlite_effect_replay_hash_conflict"
                | "postgres_effect_replay_hash_conflict"
                | "restate_effect_hash_mismatch"
        )
    }

    /// Whether retrying the identical operation is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ManagedTurnConcurrencyLimitExceeded
                | Self::RuntimeEffectGroupDrainDeferred
                | Self::SessionExecutionLaneBusy
                | Self::TurnInputSettlementSuperseded
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
                | Self::RuntimeEffectAssistantResponseHook
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
            Self::AttachmentSourcePolicyDenied
                | Self::EffectPanicked
                | Self::MissingExecutionScopeId
                | Self::ExecutionScopeTurnIdMismatch
                | Self::QueuedWorkRowExceedsContextWindow
                | Self::StoreCommitNodeBudgetExceeded
                | Self::StoreCommitByteBudgetExceeded
                | Self::CheckpointComponentEncodingVersionMismatch
                | Self::RecordEncodingFailed
                | Self::MissingProcessExecutionId
                | Self::DurableEffectLiveProtocolExtension
                | Self::DurableEffectLivePluginInput
                | Self::AwaitEventCancelUnsupported
                | Self::AwaitEventKeySign
                | Self::AwaitEventUnknownOrRevoked
                | Self::AwaitEventUnsupported
                | Self::EffectGroupUnsupported
                | Self::EffectJournalRetirementUnsupported
                | Self::InvalidAwaitEventSessionId
                | Self::InvalidAwaitEventWaitIdentity
                | Self::InvalidTurnCancelRequest
                | Self::LlmProvider
                | Self::Plugin
                | Self::PostgresEffectReplayCorruptRow
                | Self::PostgresEffectReplayDecode
                | Self::PostgresEffectReplayEncode
                | Self::PostgresEffectReplayHashConflict
                | Self::PostgresEffectReplayKeyMissing
                | Self::PostgresEffectReplayLeaseLost
                | Self::PostgresEffectReplayMissing
                | Self::PostgresEffectReplayStore
                | Self::PostgresAwaitEventDecode
                | Self::PostgresAwaitEventEncode
                | Self::PostgresAwaitEventSign
                | Self::RestateEffectController
                | Self::ProcessPanicked
                | Self::ProcessNotVisible
                | Self::ProcessAlreadyTerminal
                | Self::ProcessNoLongerRetained
                | Self::ProcessRegistryUnavailable
                | Self::ProcessSignalWaitCancelled
                | Self::ProcessSignalWaitTimeout
                | Self::RestateEffectHashMismatch
                | Self::RestateEffectHostRequiresHandlerScope
                | Self::RestateJournaledEffectPoisoned
                | Self::RestateProcessAwait
                | Self::RestateProcessAwaitAfterTurnCancel
                | Self::RestateProcessTurnCancelContextMissing
                | Self::RestateProcessTerminalEncode
                | Self::RestateTurnTerminalDecode
                | Self::RestateTurnTerminalInvalidResolution
                | Self::RestateTurnCancelScopeMismatch
                | Self::RestateTurnCancelScopeMissing
                | Self::RuntimeEffectAttachmentStore
                | Self::RuntimeEffectEnvelopeCanonicalDecode
                | Self::RuntimeEffectEnvelopeCanonicalHashInvariant
                | Self::RuntimeEffectEnvelopeHash
                | Self::RuntimeEffectGroupAwaitCancelled
                | Self::RuntimeEffectGroupChildCancelled
                | Self::RuntimeEffectGroupShape
                | Self::RuntimeEffectInvocationKind
                | Self::RuntimeEffectInvocationSubject
                | Self::RuntimeEffectLocalExecutorMismatch
                | Self::RuntimeEffectLocalExecutorUnavailable
                | Self::RuntimeEffectLocalTaskClosed
                | Self::RuntimeEffectProcessTaskJoin
                | Self::RuntimeEffectReplayRequired
                | Self::RuntimeEffectSleepCancelled
                | Self::RuntimeEffectTaskJoin
                | Self::RuntimeEffectToolAttemptCallId
                | Self::RuntimeEffectToolAttemptIndex
                | Self::RuntimeEffectToolBatchCallId
                | Self::RuntimeEffectToolBatchCallReplay
                | Self::RuntimeEffectToolBatchEmpty
                | Self::RuntimeEffectToolBatchId
                | Self::RuntimeEffectWrongOutcome
                | Self::RuntimeStoreCorrupt
                | Self::SessionCommandClaim
                | Self::SessionCommandIdempotencyKey
                | Self::SessionDeleteScopeMismatch
                | Self::SessionToolRegistry
                | Self::SqliteAwaitEventDecode
                | Self::SqliteAwaitEventEncode
                | Self::SqliteAwaitEventSign
                | Self::SqliteEffectReplayCorruptRow
                | Self::SqliteEffectReplayDecode
                | Self::SqliteEffectReplayEncode
                | Self::SqliteEffectReplayHashConflict
                | Self::SqliteEffectReplayKeyMissing
                | Self::SqliteEffectReplayLeaseLost
                | Self::SqliteEffectReplayMissing
                | Self::SqliteEffectReplayStore
                | Self::ToolBatchMissingResult
                | Self::ToolBatchResultCountMismatch
                | Self::ToolCatalogResolutionFailed
                | Self::ToolCompletionKeyMissingCallId
                | Self::ToolCompletionKeyProcessLifetime
                | Self::ToolDeferralNotDeclared
                | Self::TurnCancelGateDecode
                | Self::TurnCancelGateEncode
                | Self::TurnCancelGateInvalidTerminal
                | Self::TurnControlPeekOutcome
                | Self::TurnControlUnknownOrRevoked
                | Self::TurnTerminalDecode
                | Self::TurnTerminalEncode
                | Self::TurnTerminalInvalidResolution
                | Self::TurnTerminalUnknownOrRevoked
                | Self::TriggerStoreUnavailable
        )
    }

    /// Constructs a typed code from its stable wire representation.
    ///
    /// Built-in strings are always canonicalized to their dedicated variants;
    /// only unknown extension strings produce [`Self::ForeignCode`]. This is
    /// the supported construction path for host-defined codes.
    pub fn from_wire_code(code: &str) -> Self {
        match code {
            "attachment_source_policy_denied" => Self::AttachmentSourcePolicyDenied,
            "effect_panicked" => Self::EffectPanicked,
            "missing_execution_scope_id" => Self::MissingExecutionScopeId,
            "execution_scope_turn_id_mismatch" => Self::ExecutionScopeTurnIdMismatch,
            "managed_turn_concurrency_limit_exceeded" => Self::ManagedTurnConcurrencyLimitExceeded,
            "session_execution_lease_lost" => Self::SessionExecutionLeaseLost,
            "session_execution_lane_busy" => Self::SessionExecutionLaneBusy,
            "turn_input_settlement_superseded" => Self::TurnInputSettlementSuperseded,
            "store_commit_contended" => Self::StoreCommitContended,
            "store_commit_node_budget_exceeded" => Self::StoreCommitNodeBudgetExceeded,
            "store_commit_byte_budget_exceeded" => Self::StoreCommitByteBudgetExceeded,
            "checkpoint_component_encoding_version_mismatch" => {
                Self::CheckpointComponentEncodingVersionMismatch
            }
            "record_encoding_failed" => Self::RecordEncodingFailed,
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
            "effect_group_unsupported" => Self::EffectGroupUnsupported,
            "effect_journal_retirement_unsupported" => Self::EffectJournalRetirementUnsupported,
            "invalid_await_event_session_id" => Self::InvalidAwaitEventSessionId,
            "invalid_await_event_wait_identity" => Self::InvalidAwaitEventWaitIdentity,
            "invalid_turn_cancel_request" => Self::InvalidTurnCancelRequest,
            "live_replay" => Self::LiveReplay,
            "llm_provider" => Self::LlmProvider,
            "plugin" => Self::Plugin,
            "postgres_effect_replay_corrupt_row" => Self::PostgresEffectReplayCorruptRow,
            "postgres_effect_replay_decode" => Self::PostgresEffectReplayDecode,
            "postgres_effect_replay_encode" => Self::PostgresEffectReplayEncode,
            "postgres_effect_replay_hash_conflict" => Self::PostgresEffectReplayHashConflict,
            "postgres_effect_replay_key_missing" => Self::PostgresEffectReplayKeyMissing,
            "postgres_effect_replay_lease_lost" => Self::PostgresEffectReplayLeaseLost,
            "postgres_effect_replay_missing" => Self::PostgresEffectReplayMissing,
            "postgres_effect_replay_store" => Self::PostgresEffectReplayStore,
            "postgres_await_event_decode" => Self::PostgresAwaitEventDecode,
            "postgres_await_event_encode" => Self::PostgresAwaitEventEncode,
            "postgres_await_event_notify" => Self::PostgresAwaitEventNotify,
            "postgres_await_event_sign" => Self::PostgresAwaitEventSign,
            "postgres_await_event_store" => Self::PostgresAwaitEventStore,
            "postgres_effect_journal_retirement" => Self::PostgresEffectJournalRetirement,
            "queued_work" => Self::QueuedWork,
            "queued_work_row_exceeds_context_window" => Self::QueuedWorkRowExceedsContextWindow,
            "process_panicked" => Self::ProcessPanicked,
            "process_not_visible" => Self::ProcessNotVisible,
            "process_already_terminal" => Self::ProcessAlreadyTerminal,
            "process_no_longer_retained" => Self::ProcessNoLongerRetained,
            "process_registry_unavailable" => Self::ProcessRegistryUnavailable,
            "process_signal_wait_cancelled" => Self::ProcessSignalWaitCancelled,
            "process_signal_wait_timeout" => Self::ProcessSignalWaitTimeout,
            "restate_await_event_await" => Self::RestateAwaitEventAwait,
            "restate_await_event_cancel" => Self::RestateAwaitEventCancel,
            "restate_await_event_cancelled" => Self::RestateAwaitEventCancelled,
            "restate_await_event_peek" => Self::RestateAwaitEventPeek,
            "restate_await_event_resolve" => Self::RestateAwaitEventResolve,
            "restate_await_event_revocation_read" => Self::RestateAwaitEventRevocationRead,
            "restate_await_event_revoke" => Self::RestateAwaitEventRevoke,
            "restate_await_event_session_update" => Self::RestateAwaitEventSessionUpdate,
            "restate_effect_controller" => Self::RestateEffectController,
            "restate_effect_hash_mismatch" => Self::RestateEffectHashMismatch,
            "restate_effect_host_requires_handler_scope" => {
                Self::RestateEffectHostRequiresHandlerScope
            }
            "restate_journaled_effect_poisoned" => Self::RestateJournaledEffectPoisoned,
            "restate_process_await" => Self::RestateProcessAwait,
            "restate_process_await_after_turn_cancel" => Self::RestateProcessAwaitAfterTurnCancel,
            "restate_process_turn_cancel_context_missing" => {
                Self::RestateProcessTurnCancelContextMissing
            }
            "restate_process_terminal_encode" => Self::RestateProcessTerminalEncode,
            "restate_turn_terminal_attach" => Self::RestateTurnTerminalAttach,
            "restate_turn_terminal_attach_ceiling_elapsed" => {
                Self::RestateTurnTerminalAttachCeilingElapsed
            }
            "restate_turn_terminal_decode" => Self::RestateTurnTerminalDecode,
            "restate_turn_terminal_invalid_resolution" => {
                Self::RestateTurnTerminalInvalidResolution
            }
            "restate_turn_cancel_scope_mismatch" => Self::RestateTurnCancelScopeMismatch,
            "restate_turn_cancel_scope_missing" => Self::RestateTurnCancelScopeMissing,
            "runtime_effect_attachment_store" => Self::RuntimeEffectAttachmentStore,
            "runtime_effect_envelope_canonical_decode" => {
                Self::RuntimeEffectEnvelopeCanonicalDecode
            }
            "runtime_effect_envelope_canonical_hash_invariant" => {
                Self::RuntimeEffectEnvelopeCanonicalHashInvariant
            }
            "runtime_effect_envelope_hash" => Self::RuntimeEffectEnvelopeHash,
            "runtime_effect_group_await_cancelled" => Self::RuntimeEffectGroupAwaitCancelled,
            "runtime_effect_group_child_cancelled" => Self::RuntimeEffectGroupChildCancelled,
            "runtime_effect_group_drain_deferred" => Self::RuntimeEffectGroupDrainDeferred,
            "runtime_effect_group_shape" => Self::RuntimeEffectGroupShape,
            "runtime_effect_invocation_kind" => Self::RuntimeEffectInvocationKind,
            "runtime_effect_invocation_subject" => Self::RuntimeEffectInvocationSubject,
            "runtime_effect_local_executor_mismatch" => Self::RuntimeEffectLocalExecutorMismatch,
            "runtime_effect_local_executor_unavailable" => {
                Self::RuntimeEffectLocalExecutorUnavailable
            }
            "runtime_effect_assistant_response_hook" => Self::RuntimeEffectAssistantResponseHook,
            "runtime_effect_local_task_closed" => Self::RuntimeEffectLocalTaskClosed,
            "runtime_effect_process_task_join" => Self::RuntimeEffectProcessTaskJoin,
            "runtime_effect_replay_required" => Self::RuntimeEffectReplayRequired,
            "runtime_effect_sleep_cancelled" => Self::RuntimeEffectSleepCancelled,
            "runtime_effect_task_join" => Self::RuntimeEffectTaskJoin,
            "runtime_effect_tool_attempt_call_id" => Self::RuntimeEffectToolAttemptCallId,
            "runtime_effect_tool_attempt_index" => Self::RuntimeEffectToolAttemptIndex,
            "runtime_effect_tool_batch_call_id" => Self::RuntimeEffectToolBatchCallId,
            "runtime_effect_tool_batch_call_replay" => Self::RuntimeEffectToolBatchCallReplay,
            "runtime_effect_tool_batch_empty" => Self::RuntimeEffectToolBatchEmpty,
            "runtime_effect_tool_batch_id" => Self::RuntimeEffectToolBatchId,
            "runtime_effect_wrong_outcome" => Self::RuntimeEffectWrongOutcome,
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
            "sqlite_effect_replay_corrupt_row" => Self::SqliteEffectReplayCorruptRow,
            "sqlite_effect_replay_decode" => Self::SqliteEffectReplayDecode,
            "sqlite_effect_replay_encode" => Self::SqliteEffectReplayEncode,
            "sqlite_effect_replay_hash_conflict" => Self::SqliteEffectReplayHashConflict,
            "sqlite_effect_replay_key_missing" => Self::SqliteEffectReplayKeyMissing,
            "sqlite_effect_replay_lease_lost" => Self::SqliteEffectReplayLeaseLost,
            "sqlite_effect_replay_missing" => Self::SqliteEffectReplayMissing,
            "sqlite_effect_replay_store" => Self::SqliteEffectReplayStore,
            "tool_batch_missing_result" => Self::ToolBatchMissingResult,
            "tool_batch_result_count_mismatch" => Self::ToolBatchResultCountMismatch,
            "tool_catalog_resolution_failed" => Self::ToolCatalogResolutionFailed,
            "tool_completion_key_missing_call_id" => Self::ToolCompletionKeyMissingCallId,
            "tool_completion_key_process_lifetime" => Self::ToolCompletionKeyProcessLifetime,
            "tool_deferral_not_declared" => Self::ToolDeferralNotDeclared,
            "transient_cancel_watch" => Self::TransientCancelWatch,
            "transient_terminal_publication" => Self::TransientTerminalPublication,
            "turn_cancel_gate_decode" => Self::TurnCancelGateDecode,
            "turn_cancel_gate_encode" => Self::TurnCancelGateEncode,
            "turn_cancel_gate_invalid_terminal" => Self::TurnCancelGateInvalidTerminal,
            "turn_control_peek_outcome" => Self::TurnControlPeekOutcome,
            "turn_control_unknown_or_revoked" => Self::TurnControlUnknownOrRevoked,
            "turn_control_wait_cancelled" => Self::TurnControlWaitCancelled,
            "turn_control_wait_timeout" => Self::TurnControlWaitTimeout,
            "turn_terminal_await_timeout" => Self::TurnTerminalAwaitTimeout,
            "turn_terminal_decode" => Self::TurnTerminalDecode,
            "turn_terminal_encode" => Self::TurnTerminalEncode,
            "turn_terminal_invalid_resolution" => Self::TurnTerminalInvalidResolution,
            "turn_terminal_unknown_or_revoked" => Self::TurnTerminalUnknownOrRevoked,
            "trigger_store_unavailable" => Self::TriggerStoreUnavailable,
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
    /// Structured, content-free evidence for a replay mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::RuntimeEffectReplayMismatchReport>,
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
            summary: None,
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
    fn replay_mismatch_classification_covers_every_durable_controller_code() {
        for code in [
            "sqlite_effect_replay_hash_conflict",
            "postgres_effect_replay_hash_conflict",
            "restate_effect_hash_mismatch",
        ] {
            let typed = RuntimeErrorCode::from_wire_code(code);
            assert!(typed.is_replay_mismatch(), "{code}");
            assert_eq!(
                typed.as_str(),
                code,
                "classification must preserve display code"
            );
        }
    }

    #[test]
    fn nearby_mismatch_codes_are_not_replay_divergence() {
        for code in [
            "runtime_effect_envelope_canonical_hash_invariant",
            "runtime_effect_local_executor_mismatch",
        ] {
            assert!(
                !RuntimeErrorCode::from_wire_code(code).is_replay_mismatch(),
                "{code}"
            );
        }
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
            // A hook failure is an incomplete derivation over an already
            // durable completion, so redriving phase 2 is the correct recovery
            // (FIG-1276).
            RuntimeErrorCode::RuntimeEffectAssistantResponseHook
            | RuntimeErrorCode::RuntimeEffectGroupDrainDeferred
            | RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded
            | RuntimeErrorCode::SessionExecutionLaneBusy
            | RuntimeErrorCode::TurnInputSettlementSuperseded
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
            RuntimeErrorCode::AttachmentSourcePolicyDenied
            | RuntimeErrorCode::EffectPanicked
            | RuntimeErrorCode::MissingExecutionScopeId
            | RuntimeErrorCode::ExecutionScopeTurnIdMismatch
            | RuntimeErrorCode::QueuedWorkRowExceedsContextWindow
            | RuntimeErrorCode::StoreCommitNodeBudgetExceeded
            | RuntimeErrorCode::StoreCommitByteBudgetExceeded
            | RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch
            | RuntimeErrorCode::RecordEncodingFailed
            | RuntimeErrorCode::MissingProcessExecutionId
            | RuntimeErrorCode::DurableEffectLiveProtocolExtension
            | RuntimeErrorCode::DurableEffectLivePluginInput
            | RuntimeErrorCode::AwaitEventCancelUnsupported
            | RuntimeErrorCode::AwaitEventKeySign
            | RuntimeErrorCode::AwaitEventUnknownOrRevoked
            | RuntimeErrorCode::AwaitEventUnsupported
            | RuntimeErrorCode::EffectGroupUnsupported
            | RuntimeErrorCode::EffectJournalRetirementUnsupported
            | RuntimeErrorCode::InvalidAwaitEventSessionId
            | RuntimeErrorCode::InvalidAwaitEventWaitIdentity
            | RuntimeErrorCode::InvalidTurnCancelRequest
            | RuntimeErrorCode::LlmProvider
            | RuntimeErrorCode::Plugin
            | RuntimeErrorCode::PostgresEffectReplayCorruptRow
            | RuntimeErrorCode::PostgresEffectReplayDecode
            | RuntimeErrorCode::PostgresEffectReplayEncode
            | RuntimeErrorCode::PostgresEffectReplayHashConflict
            | RuntimeErrorCode::PostgresEffectReplayKeyMissing
            | RuntimeErrorCode::PostgresEffectReplayLeaseLost
            | RuntimeErrorCode::PostgresEffectReplayMissing
            | RuntimeErrorCode::PostgresEffectReplayStore
            | RuntimeErrorCode::PostgresAwaitEventDecode
            | RuntimeErrorCode::PostgresAwaitEventEncode
            | RuntimeErrorCode::PostgresAwaitEventSign
            | RuntimeErrorCode::RestateEffectController
            | RuntimeErrorCode::ProcessPanicked
            | RuntimeErrorCode::ProcessNotVisible
            | RuntimeErrorCode::ProcessAlreadyTerminal
            | RuntimeErrorCode::ProcessNoLongerRetained
            | RuntimeErrorCode::ProcessRegistryUnavailable
            | RuntimeErrorCode::ProcessSignalWaitCancelled
            | RuntimeErrorCode::ProcessSignalWaitTimeout
            | RuntimeErrorCode::RestateEffectHashMismatch
            | RuntimeErrorCode::RestateEffectHostRequiresHandlerScope
            | RuntimeErrorCode::RestateJournaledEffectPoisoned
            | RuntimeErrorCode::RestateProcessAwait
            | RuntimeErrorCode::RestateProcessAwaitAfterTurnCancel
            | RuntimeErrorCode::RestateProcessTurnCancelContextMissing
            | RuntimeErrorCode::RestateProcessTerminalEncode
            | RuntimeErrorCode::RestateTurnTerminalDecode
            | RuntimeErrorCode::RestateTurnTerminalInvalidResolution
            | RuntimeErrorCode::RestateTurnCancelScopeMismatch
            | RuntimeErrorCode::RestateTurnCancelScopeMissing
            | RuntimeErrorCode::RuntimeEffectAttachmentStore
            | RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode
            | RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant
            | RuntimeErrorCode::RuntimeEffectEnvelopeHash
            | RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled
            | RuntimeErrorCode::RuntimeEffectGroupChildCancelled
            | RuntimeErrorCode::RuntimeEffectGroupShape
            | RuntimeErrorCode::RuntimeEffectInvocationKind
            | RuntimeErrorCode::RuntimeEffectInvocationSubject
            | RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch
            | RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable
            | RuntimeErrorCode::RuntimeEffectLocalTaskClosed
            | RuntimeErrorCode::RuntimeEffectProcessTaskJoin
            | RuntimeErrorCode::RuntimeEffectReplayRequired
            | RuntimeErrorCode::RuntimeEffectSleepCancelled
            | RuntimeErrorCode::RuntimeEffectTaskJoin
            | RuntimeErrorCode::RuntimeEffectToolAttemptCallId
            | RuntimeErrorCode::RuntimeEffectToolAttemptIndex
            | RuntimeErrorCode::RuntimeEffectToolBatchCallId
            | RuntimeErrorCode::RuntimeEffectToolBatchCallReplay
            | RuntimeErrorCode::RuntimeEffectToolBatchEmpty
            | RuntimeErrorCode::RuntimeEffectToolBatchId
            | RuntimeErrorCode::RuntimeEffectWrongOutcome
            | RuntimeErrorCode::RuntimeStoreCorrupt
            | RuntimeErrorCode::SessionCommandClaim
            | RuntimeErrorCode::SessionCommandIdempotencyKey
            | RuntimeErrorCode::SessionDeleteScopeMismatch
            | RuntimeErrorCode::SessionToolRegistry
            | RuntimeErrorCode::SqliteAwaitEventDecode
            | RuntimeErrorCode::SqliteAwaitEventEncode
            | RuntimeErrorCode::SqliteAwaitEventSign
            | RuntimeErrorCode::SqliteEffectReplayCorruptRow
            | RuntimeErrorCode::SqliteEffectReplayDecode
            | RuntimeErrorCode::SqliteEffectReplayEncode
            | RuntimeErrorCode::SqliteEffectReplayHashConflict
            | RuntimeErrorCode::SqliteEffectReplayKeyMissing
            | RuntimeErrorCode::SqliteEffectReplayLeaseLost
            | RuntimeErrorCode::SqliteEffectReplayMissing
            | RuntimeErrorCode::SqliteEffectReplayStore
            | RuntimeErrorCode::ToolBatchMissingResult
            | RuntimeErrorCode::ToolBatchResultCountMismatch
            | RuntimeErrorCode::ToolCatalogResolutionFailed
            | RuntimeErrorCode::ToolCompletionKeyMissingCallId
            | RuntimeErrorCode::ToolCompletionKeyProcessLifetime
            | RuntimeErrorCode::ToolDeferralNotDeclared
            | RuntimeErrorCode::TurnCancelGateDecode
            | RuntimeErrorCode::TurnCancelGateEncode
            | RuntimeErrorCode::TurnCancelGateInvalidTerminal
            | RuntimeErrorCode::TurnControlPeekOutcome
            | RuntimeErrorCode::TurnControlUnknownOrRevoked
            | RuntimeErrorCode::TurnTerminalDecode
            | RuntimeErrorCode::TurnTerminalEncode
            | RuntimeErrorCode::TurnTerminalInvalidResolution
            | RuntimeErrorCode::TurnTerminalUnknownOrRevoked
            | RuntimeErrorCode::TriggerStoreUnavailable => ExpectedClassification::Terminal,
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
        let first_party_codes = [
            RuntimeErrorCode::AttachmentSourcePolicyDenied,
            RuntimeErrorCode::EffectPanicked,
            RuntimeErrorCode::MissingExecutionScopeId,
            RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
            RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded,
            RuntimeErrorCode::SessionExecutionLeaseLost,
            RuntimeErrorCode::SessionExecutionLaneBusy,
            RuntimeErrorCode::TurnInputSettlementSuperseded,
            RuntimeErrorCode::StoreCommitContended,
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            RuntimeErrorCode::StoreCommitByteBudgetExceeded,
            RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch,
            RuntimeErrorCode::RecordEncodingFailed,
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
            RuntimeErrorCode::EffectGroupUnsupported,
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            RuntimeErrorCode::InvalidAwaitEventSessionId,
            RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
            RuntimeErrorCode::InvalidTurnCancelRequest,
            RuntimeErrorCode::LiveReplay,
            RuntimeErrorCode::LlmProvider,
            RuntimeErrorCode::Plugin,
            RuntimeErrorCode::PostgresEffectReplayCorruptRow,
            RuntimeErrorCode::PostgresEffectReplayDecode,
            RuntimeErrorCode::PostgresEffectReplayEncode,
            RuntimeErrorCode::PostgresEffectReplayHashConflict,
            RuntimeErrorCode::PostgresEffectReplayKeyMissing,
            RuntimeErrorCode::PostgresEffectReplayLeaseLost,
            RuntimeErrorCode::PostgresEffectReplayMissing,
            RuntimeErrorCode::PostgresEffectReplayStore,
            RuntimeErrorCode::PostgresAwaitEventDecode,
            RuntimeErrorCode::PostgresAwaitEventEncode,
            RuntimeErrorCode::PostgresAwaitEventNotify,
            RuntimeErrorCode::PostgresAwaitEventSign,
            RuntimeErrorCode::PostgresAwaitEventStore,
            RuntimeErrorCode::PostgresEffectJournalRetirement,
            RuntimeErrorCode::QueuedWork,
            RuntimeErrorCode::QueuedWorkRowExceedsContextWindow,
            RuntimeErrorCode::ProcessPanicked,
            RuntimeErrorCode::ProcessNotVisible,
            RuntimeErrorCode::ProcessAlreadyTerminal,
            RuntimeErrorCode::ProcessNoLongerRetained,
            RuntimeErrorCode::ProcessRegistryUnavailable,
            RuntimeErrorCode::ProcessSignalWaitCancelled,
            RuntimeErrorCode::ProcessSignalWaitTimeout,
            RuntimeErrorCode::RestateAwaitEventAwait,
            RuntimeErrorCode::RestateAwaitEventCancel,
            RuntimeErrorCode::RestateAwaitEventCancelled,
            RuntimeErrorCode::RestateAwaitEventPeek,
            RuntimeErrorCode::RestateAwaitEventResolve,
            RuntimeErrorCode::RestateAwaitEventRevocationRead,
            RuntimeErrorCode::RestateAwaitEventRevoke,
            RuntimeErrorCode::RestateAwaitEventSessionUpdate,
            RuntimeErrorCode::RestateEffectController,
            RuntimeErrorCode::RestateEffectHashMismatch,
            RuntimeErrorCode::RestateEffectHostRequiresHandlerScope,
            RuntimeErrorCode::RestateJournaledEffectPoisoned,
            RuntimeErrorCode::RestateProcessAwait,
            RuntimeErrorCode::RestateProcessAwaitAfterTurnCancel,
            RuntimeErrorCode::RestateProcessTurnCancelContextMissing,
            RuntimeErrorCode::RestateProcessTerminalEncode,
            RuntimeErrorCode::RestateTurnTerminalAttach,
            RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed,
            RuntimeErrorCode::RestateTurnTerminalDecode,
            RuntimeErrorCode::RestateTurnTerminalInvalidResolution,
            RuntimeErrorCode::RestateTurnCancelScopeMismatch,
            RuntimeErrorCode::RestateTurnCancelScopeMissing,
            RuntimeErrorCode::RuntimeEffectAttachmentStore,
            RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode,
            RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant,
            RuntimeErrorCode::RuntimeEffectEnvelopeHash,
            RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
            RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
            RuntimeErrorCode::RuntimeEffectGroupDrainDeferred,
            RuntimeErrorCode::RuntimeEffectGroupShape,
            RuntimeErrorCode::RuntimeEffectInvocationKind,
            RuntimeErrorCode::RuntimeEffectInvocationSubject,
            RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
            RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
            RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
            RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
            RuntimeErrorCode::RuntimeEffectProcessTaskJoin,
            RuntimeErrorCode::RuntimeEffectReplayRequired,
            RuntimeErrorCode::RuntimeEffectSleepCancelled,
            RuntimeErrorCode::RuntimeEffectTaskJoin,
            RuntimeErrorCode::RuntimeEffectToolAttemptCallId,
            RuntimeErrorCode::RuntimeEffectToolAttemptIndex,
            RuntimeErrorCode::RuntimeEffectToolBatchCallId,
            RuntimeErrorCode::RuntimeEffectToolBatchCallReplay,
            RuntimeErrorCode::RuntimeEffectToolBatchEmpty,
            RuntimeErrorCode::RuntimeEffectToolBatchId,
            RuntimeErrorCode::RuntimeEffectWrongOutcome,
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
            RuntimeErrorCode::SqliteEffectReplayCorruptRow,
            RuntimeErrorCode::SqliteEffectReplayDecode,
            RuntimeErrorCode::SqliteEffectReplayEncode,
            RuntimeErrorCode::SqliteEffectReplayHashConflict,
            RuntimeErrorCode::SqliteEffectReplayKeyMissing,
            RuntimeErrorCode::SqliteEffectReplayLeaseLost,
            RuntimeErrorCode::SqliteEffectReplayMissing,
            RuntimeErrorCode::SqliteEffectReplayStore,
            RuntimeErrorCode::ToolBatchMissingResult,
            RuntimeErrorCode::ToolBatchResultCountMismatch,
            RuntimeErrorCode::ToolCatalogResolutionFailed,
            RuntimeErrorCode::ToolCompletionKeyMissingCallId,
            RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
            RuntimeErrorCode::ToolDeferralNotDeclared,
            RuntimeErrorCode::TransientCancelWatch,
            RuntimeErrorCode::TransientTerminalPublication,
            RuntimeErrorCode::TurnCancelGateDecode,
            RuntimeErrorCode::TurnCancelGateEncode,
            RuntimeErrorCode::TurnCancelGateInvalidTerminal,
            RuntimeErrorCode::TurnControlPeekOutcome,
            RuntimeErrorCode::TurnControlUnknownOrRevoked,
            RuntimeErrorCode::TurnControlWaitCancelled,
            RuntimeErrorCode::TurnControlWaitTimeout,
            RuntimeErrorCode::TurnTerminalAwaitTimeout,
            RuntimeErrorCode::TurnTerminalDecode,
            RuntimeErrorCode::TurnTerminalEncode,
            RuntimeErrorCode::TurnTerminalInvalidResolution,
            RuntimeErrorCode::TurnTerminalUnknownOrRevoked,
            RuntimeErrorCode::TriggerStoreUnavailable,
        ];

        for code in first_party_codes {
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
            assert!(
                !matches!(&decoded, RuntimeErrorCode::ForeignCode(_)),
                "first-party code {} decoded as foreign",
                code.as_str()
            );
            assert_eq!(decoded, code, "typed round trip for {code}");
        }

        let foreign = RuntimeErrorCode::from_wire_code("plugin_defined_abort");
        assert_eq!(
            expected_classification(&foreign),
            ExpectedClassification::Unknown
        );
    }

    /// A failed assistant-response hook is an incomplete derivation over a
    /// completion the journal already holds, so the only correct recovery is to
    /// redrive phase 2 (FIG-1276). That is a claim about `is_retryable`, not
    /// merely about staying out of `is_terminal`: an unclassified code is
    /// `Unknown`, which durable hosts are free to settle either way.
    #[test]
    fn assistant_response_hook_failures_are_retryable_not_terminal() {
        let code = RuntimeErrorCode::RuntimeEffectAssistantResponseHook;

        assert!(code.is_retryable(), "phase 2 must be redrivable");
        assert!(!code.is_terminal());
        assert_eq!(code.as_str(), "runtime_effect_assistant_response_hook");

        let error = RuntimeError::new(code.clone(), "assistant response hook failed");
        assert!(error.is_retryable());
        assert!(!error.is_terminal());
        assert_eq!(
            RuntimeErrorCode::from_wire_code("runtime_effect_assistant_response_hook"),
            code,
            "the wire code must decode as first-party, not foreign"
        );
    }

    #[test]
    fn unsafe_effect_replay_and_durable_timeout_codes_are_terminal() {
        for code in [
            RuntimeErrorCode::PostgresEffectReplayLeaseLost,
            RuntimeErrorCode::SqliteEffectReplayLeaseLost,
            RuntimeErrorCode::ProcessSignalWaitTimeout,
        ] {
            assert!(!code.is_retryable(), "{code} must not be retried");
            assert!(code.is_terminal(), "{code} must settle terminally");
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
