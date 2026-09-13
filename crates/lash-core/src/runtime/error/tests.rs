use super::{RuntimeError, RuntimeErrorCode};
use crate::SessionId;

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
        "worker_replacement_abort",
        "tool_intent_replay_key_format_cutover",
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
fn retired_restate_hash_mismatch_wire_code_decodes_to_the_current_classification() {
    let code = RuntimeErrorCode::from_wire_code("restate_effect_hash_mismatch");

    assert_eq!(code, RuntimeErrorCode::WorkerReplacementAbort);
    assert_eq!(code.as_str(), "worker_replacement_abort");
    assert!(code.is_replay_mismatch());
    assert!(code.is_terminal());
    let encoded = serde_json::to_value(&code).expect("serialize retired wire code");
    assert_eq!(encoded, serde_json::json!("worker_replacement_abort"));
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
        | RuntimeErrorCode::RestateProcessCancel
        | RuntimeErrorCode::RestateProcessIngressSubmit
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
        | RuntimeErrorCode::TurnInputRedriveSetUnavailable
        | RuntimeErrorCode::QueuedWorkRowExceedsContextWindow
        | RuntimeErrorCode::StoreCommitNodeBudgetExceeded
        | RuntimeErrorCode::StoreCommitByteBudgetExceeded
        | RuntimeErrorCode::SessionDeleted
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
        | RuntimeErrorCode::EffectScopeRetired
        | RuntimeErrorCode::EffectScopeNotQuiescent
        | RuntimeErrorCode::AwaitEventScopeNotRetirable
        | RuntimeErrorCode::InvalidAwaitEventSessionId
        | RuntimeErrorCode::InvalidAwaitEventWaitIdentity
        | RuntimeErrorCode::InvalidTurnCancelRequest
        | RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
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
        | RuntimeErrorCode::ProcessParentEnded
        | RuntimeErrorCode::ProcessCancelConflict
        | RuntimeErrorCode::ProcessNoLongerRetained
        | RuntimeErrorCode::ProcessIncarnationSuperseded
        | RuntimeErrorCode::ProcessRegistryUnavailable
        | RuntimeErrorCode::ProcessSignalWaitCancelled
        | RuntimeErrorCode::ProcessSignalWaitTimeout
        | RuntimeErrorCode::WorkerReplacementAbort
        | RuntimeErrorCode::ToolIntentReplayKeyFormatCutover
        | RuntimeErrorCode::RestateEffectHostRequiresHandlerScope
        | RuntimeErrorCode::RestateJournaledEffectPoisoned
        | RuntimeErrorCode::RestateProcessAwait
        | RuntimeErrorCode::RestateProcessJournalIdentityDrift
        | RuntimeErrorCode::RestateProcessJournalPayloadIncompatible
        | RuntimeErrorCode::RestateServiceUnregistered
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
        | RuntimeErrorCode::RuntimeEffectInvocationSubject
        | RuntimeErrorCode::RuntimeEffectScopeMismatch
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
        | RuntimeErrorCode::StoreCommitSuperseded
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
        RuntimeErrorCode::TurnInputRedriveSetUnavailable,
        RuntimeErrorCode::StoreCommitContended,
        RuntimeErrorCode::StoreCommitSuperseded,
        RuntimeErrorCode::SessionDeleted,
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
        RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
        RuntimeErrorCode::DurableEffectLiveProtocolExtension,
        RuntimeErrorCode::DurableEffectLivePluginInput,
        RuntimeErrorCode::AwaitEventCancelUnsupported,
        RuntimeErrorCode::AwaitEventKeySign,
        RuntimeErrorCode::AwaitEventUnknownOrRevoked,
        RuntimeErrorCode::AwaitEventUnsupported,
        RuntimeErrorCode::CancelStartGateUnavailable,
        RuntimeErrorCode::EffectGroupUnsupported,
        RuntimeErrorCode::EffectJournalRetirementUnsupported,
        RuntimeErrorCode::EffectScopeRetired,
        RuntimeErrorCode::EffectScopeNotQuiescent,
        RuntimeErrorCode::AwaitEventScopeNotRetirable,
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
        RuntimeErrorCode::ProcessParentEnded,
        RuntimeErrorCode::ProcessCancelConflict,
        RuntimeErrorCode::ProcessNoLongerRetained,
        RuntimeErrorCode::ProcessIncarnationSuperseded,
        RuntimeErrorCode::ProcessRegistryUnavailable,
        RuntimeErrorCode::ProcessSignalWaitCancelled,
        RuntimeErrorCode::ProcessSignalWaitTimeout,
        RuntimeErrorCode::RestateAwaitEventAwait,
        RuntimeErrorCode::RestateAwaitEventCancel,
        RuntimeErrorCode::RestateAwaitEventPeek,
        RuntimeErrorCode::RestateAwaitEventResolve,
        RuntimeErrorCode::RestateAwaitEventRevocationRead,
        RuntimeErrorCode::RestateAwaitEventRevoke,
        RuntimeErrorCode::RestateAwaitEventSessionUpdate,
        RuntimeErrorCode::RestateEffectController,
        RuntimeErrorCode::WorkerReplacementAbort,
        RuntimeErrorCode::ToolIntentReplayKeyFormatCutover,
        RuntimeErrorCode::RestateEffectHostRequiresHandlerScope,
        RuntimeErrorCode::RestateJournaledEffectPoisoned,
        RuntimeErrorCode::RestateProcessAwait,
        RuntimeErrorCode::RestateProcessCancel,
        RuntimeErrorCode::RestateProcessJournalIdentityDrift,
        RuntimeErrorCode::RestateProcessJournalPayloadIncompatible,
        RuntimeErrorCode::RestateProcessIngressSubmit,
        RuntimeErrorCode::RestateServiceUnregistered,
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
        RuntimeErrorCode::RuntimeEffectInvocationSubject,
        RuntimeErrorCode::RuntimeEffectScopeMismatch,
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
    let error = RuntimeError::new(RuntimeErrorCode::RuntimeStore, "session deleted").with_cause(
        super::RuntimeErrorCause::SessionDeleted {
            session_id: SessionId::from("retired"),
        },
    );

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
