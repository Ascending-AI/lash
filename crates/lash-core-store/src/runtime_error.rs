//! Kernel error vocabulary.
//!
//! `RuntimeError` and its code enum are the typed failure surface every
//! durable operation reports, and they name `StoreError` directly, so they
//! live beside it. The session-facing mapping into `SessionError` stays in
//! `lash-core`.

use crate::{RuntimeEffectKind, SessionId};
use serde::{Deserialize, Serialize};

/// Stable runtime error code.
///
/// Codes serialize as the same snake_case strings exposed in traces and host
/// errors, but callers should match this type instead of parsing display text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuntimeErrorCode {
    AttachmentSourcePolicyDenied,
    /// An artifact write named an owner a permanent retirement fence has
    /// already closed. Store implementors return this code instead of
    /// wording the refusal as prose; the destination-owner form of the same
    /// fence is [`Self::ArtifactDestinationOwnerRetired`].
    ArtifactOwnerRetired,
    /// An artifact transfer named a destination owner a permanent retirement
    /// fence has already closed. Kept as its own code so the retirement
    /// target is a fact, not a word that must appear in the message.
    ArtifactDestinationOwnerRetired,
    /// An artifact transfer found neither the staging owner's edge nor the
    /// destination owner's edge. Store implementors return this code so the
    /// caller that staged the bytes can settle the destination edge itself.
    ArtifactStagingEdgeMissing,
    EffectPanicked,
    MissingExecutionScopeId,
    ExecutionScopeTurnIdMismatch,
    /// An execution scope was admitted for effect work without the process
    /// incarnation its scope kind requires (or pinned to an incarnation that
    /// is not its own). Retrying the identical admission fails identically;
    /// the caller must admit the scope through the authority that owns the
    /// process incarnation.
    ExecutionScopeAdmissionRefused,
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
    /// A replayed acceptance was already settled, but the durable application
    /// records needed to reconstruct its original turn-input set were missing,
    /// unreadable, or inconsistent. Retrying unchanged cannot repair the
    /// history; an operator must restore the application records before
    /// redriving the same turn.
    TurnInputRedriveSetUnavailable,
    /// A turn was attempted on a runtime opened with
    /// `ToolSurfaceOpenMode::PreservePersisted` (FIG-3353). That open declared
    /// it would not run a turn: its tool surface was never reconciled and no
    /// `ToolSourcePolicy` was enforced, so no direct or queued turn may
    /// execute against it. Reopening the session in `Reconcile` mode is the
    /// recovery; retrying the identical call on this open fails identically.
    TurnExecutionRequiresReconciledToolSurface,
    /// The store aborted a commit before publication because transactional
    /// write authority was contended. Retrying the same operation unchanged is
    /// safe; reloading or rebasing is not required.
    StoreCommitContended,
    /// The final runtime commit lost the session-head compare-and-swap to a
    /// newer commit. Nothing from the losing commit was published, but the
    /// identical stale commit is not safe to retry: reload the durable head and
    /// re-establish current lease and claim authority before building new work
    /// (ADR 0029).
    StoreCommitSuperseded,
    /// The session was deleted before its final runtime commit could publish.
    /// The session id is also retained in [`RuntimeErrorCause::SessionDeleted`]
    /// so hosts need not recover structured identity from display text.
    SessionDeleted,
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
    /// A persisted historical frame cannot become resident through this API:
    /// switching it would replace resident configuration without a commanded
    /// config patch, and no such patch supports historical-frame switching.
    HistoricalAgentFrameSwitchUnsupported,
    /// Two authors named a different agent-frame switch for one turn, or named
    /// the same frame with different seed nodes. A turn materializes at most
    /// one switch and there is no precedence order between its authors, so the
    /// commit is refused before any durable write. The identical turn fails
    /// identically until one of the two authors stops switching.
    AgentFrameSwitchAuthorConflict,
    DurableEffectLiveProtocolExtension,
    DurableEffectLivePluginInput,
    AwaitEventCancelUnsupported,
    AwaitEventKeySign,
    AwaitEventUnknownOrRevoked,
    AwaitEventUnsupported,
    CancelStartGateUnavailable,
    EffectGroupUnsupported,
    EffectJournalRetirementUnsupported,
    EffectScopeRetired,
    EffectScopeNotQuiescent,
    AwaitEventScopeNotRetirable,
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
    /// Effect-host implementor diagnostic for a child registration refused
    /// because its declared parent scope has already ended.
    ProcessParentEnded,
    /// Effect-host implementor diagnostic for a conflicting cancellation request.
    ProcessCancelConflict,
    /// A durable identity was re-presented with content the store already
    /// holds different content for.
    ///
    /// The stores fence re-submitted identities at the point they mutate: a
    /// process registration fingerprint, a process-event replay key, a trigger
    /// occurrence idempotency key. Matching content replays the first writer's
    /// result; differing content cannot, because the identity is already bound.
    /// Retrying the same changed payload fails identically, so this is terminal
    /// rather than retryable. Hosts see it as one refusal vocabulary at the
    /// tool-intent front door (FIG-1489).
    DurableIdentityConflict,
    /// ADR 0051 effect-host implementor diagnostic for a process-command
    /// refusal whose terminal target has been replaced by a retention tombstone.
    ProcessNoLongerRetained,
    ProcessIncarnationSuperseded,
    ProcessRegistryUnavailable,
    ProcessSignalWaitCancelled,
    ProcessSignalWaitTimeout,
    RestateAwaitEventAwait,
    RestateAwaitEventCancel,
    RestateAwaitEventPeek,
    RestateAwaitEventResolve,
    RestateAwaitEventRevocationRead,
    RestateAwaitEventRevoke,
    RestateAwaitEventSessionUpdate,
    RestateEffectController,
    /// Replay found a retired tool-intent v1 key; re-execution under v2 could
    /// duplicate or diverge from the committed command, so it is refused.
    ToolIntentReplayKeyFormatCutover,
    /// A Restate redrive diverged from its durable journal and cannot replay it
    /// safely; a fresh turn on the same session is safe.
    WorkerReplacementAbort,
    RestateEffectHostRequiresHandlerScope,
    /// A journaled Restate effect produced an unacceptable outcome and became
    /// terminal rather than failing every enclosing-turn redrive.
    RestateJournaledEffectPoisoned,
    RestateProcessAwait,
    RestateProcessCancel,
    /// A Restate DirectProcess redrive addressed an existing journal entry
    /// with a different canonical process-command identity.
    RestateProcessJournalIdentityDrift,
    /// A Restate DirectProcess journal entry has an unsupported version or a
    /// shape this build cannot decode exactly.
    RestateProcessJournalPayloadIncompatible,
    RestateProcessIngressSubmit,
    /// The ingress target names an unbound service; retry cannot change that
    /// deployment fact, so this code is terminal.
    RestateServiceUnregistered,
    RestateProcessAwaitAfterTurnCancel,
    RestateProcessTurnCancelContextMissing,
    RestateProcessTerminalEncode,
    RestateTurnTerminalAttach,
    /// A Restate terminal attachment elapsed; re-attaching is safe.
    RestateTurnTerminalAttachCeilingElapsed,
    RestateTurnTerminalDecode,
    RestateTurnTerminalInvalidResolution,
    RestateTurnCancelScopeMismatch,
    RestateTurnCancelScopeMissing,
    /// A journaled response hook retries derivation without paying again (FIG-1276).
    RuntimeEffectAssistantResponseHook,
    RuntimeEffectAttachmentStore,
    RuntimeEffectEnvelopeCanonicalDecode,
    RuntimeEffectEnvelopeCanonicalHashInvariant,
    RuntimeEffectEnvelopeHash,
    RuntimeEffectEnvelopeVersion,
    /// A cancelled group await leaves its durable rank untouched for retry.
    RuntimeEffectGroupAwaitCancelled,
    /// A group's `Cancel` loser disposition made this child terminal.
    RuntimeEffectGroupChildCancelled,
    /// The child's cancel decision already won the group's durable
    /// linearization point, so its late final record was refused and nothing
    /// was journaled (ADR 0099 §4, W17).
    RuntimeEffectGroupChildCancelDecided,
    /// Drain deferred while this host still works the group or its children.
    /// Retry succeeds once it finishes; permanent refusal uses
    /// `RuntimeEffectGroupShape`.
    RuntimeEffectGroupDrainDeferred,
    /// A durable effect group was assembled with children that disagree with the
    /// group they claim to belong to, or an effect carrying group membership
    /// reached a command shape that cannot honor it.
    RuntimeEffectGroupShape,
    RuntimeEffectInvocationSubject,
    RuntimeEffectScopeMismatch,
    RuntimeEffectLocalExecutorMismatch,
    RuntimeEffectLocalExecutorUnavailable,
    RuntimeEffectLocalTaskClosed,
    RuntimeEffectProcessTaskJoin,
    RuntimeEffectReplayRequired,
    RuntimeEffectSleepCancelled,
    RuntimeEffectTaskJoin,
    RuntimeEffectToolAttemptCallId,
    RuntimeEffectToolAttemptCaptureVersion,
    RuntimeEffectToolAttemptIndex,
    RuntimeEffectToolBatchCallId,
    RuntimeEffectToolBatchCallReplay,
    RuntimeEffectToolBatchEmpty,
    RuntimeEffectToolBatchId,
    RuntimeEffectToolChildCancellationAuthority,
    RuntimeEffectToolChildCompletionRouting,
    RuntimeEffectToolChildRequestAdmission,
    RuntimeEffectToolChildRequestCallId,
    RuntimeEffectToolChildRequestOpener,
    RuntimeEffectToolChildRequestVersion,
    RuntimeEffectToolSettlementVersion,
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
pub fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    match err {
        crate::store::StoreError::Contended => RuntimeError::new(
            RuntimeErrorCode::StoreCommitContended,
            "store commit is contended; retry the identical operation unchanged",
        ),
        err @ crate::store::StoreError::HeadRevisionConflict { .. } => RuntimeError::new(
            RuntimeErrorCode::StoreCommitSuperseded,
            format!(
                "{err}; reload the durable head and re-establish lease and claim authority before retrying"
            ),
        ),
        err @ crate::store::StoreError::TurnCancelIntentChanged { .. } => {
            RuntimeError::new(RuntimeErrorCode::StoreCommitSuperseded, err.to_string())
        }
        ref err @ crate::store::StoreError::SessionDeleted { ref session_id } => {
            RuntimeError::new(RuntimeErrorCode::SessionDeleted, err.to_string()).with_cause(
                RuntimeErrorCause::SessionDeleted {
                    session_id: session_id.clone(),
                },
            )
        }
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
        // A no-claim driver that finds the row held/settled cedes at head CAS;
        // nothing was written, so this is stand-down (ADR 0069 §5(d)).
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
        crate::store::StoreError::TurnOutcomeMaterializationRefused { error } => *error,
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
}
/// The decided retry posture of a [`RuntimeErrorCode`].
///
/// `Unclassified` means no retry posture is decided: the code is neither
/// explicitly safe to retry unchanged nor provably permanent, and durable
/// hosts may settle it either way. Foreign codes land here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeErrorClass {
    /// Retrying the identical operation is explicitly safe.
    Retryable,
    /// Retrying cannot succeed without changing input, configuration,
    /// wiring, or corrupted durable state.
    Terminal,
    /// No decided retry posture.
    Unclassified,
}
impl RuntimeErrorCode {
    /// Provides the canonical str view to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn as_str(&self) -> &str {
        match self {
            Self::AttachmentSourcePolicyDenied => "attachment_source_policy_denied",
            Self::ArtifactOwnerRetired => "artifact_owner_retired",
            Self::ArtifactDestinationOwnerRetired => "artifact_destination_owner_retired",
            Self::ArtifactStagingEdgeMissing => "artifact_staging_edge_missing",
            Self::EffectPanicked => "effect_panicked",
            Self::MissingExecutionScopeId => "missing_execution_scope_id",
            Self::ExecutionScopeTurnIdMismatch => "execution_scope_turn_id_mismatch",
            Self::ExecutionScopeAdmissionRefused => "execution_scope_admission_refused",
            Self::SessionExecutionLeaseLost => "session_execution_lease_lost",
            Self::SessionExecutionLaneBusy => "session_execution_lane_busy",
            Self::TurnInputSettlementSuperseded => "turn_input_settlement_superseded",
            Self::TurnInputRedriveSetUnavailable => "turn_input_redrive_set_unavailable",
            Self::TurnExecutionRequiresReconciledToolSurface => {
                "turn_execution_requires_reconciled_tool_surface"
            }
            Self::StoreCommitContended => "store_commit_contended",
            Self::StoreCommitSuperseded => "store_commit_superseded",
            Self::SessionDeleted => "session_deleted",
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
            Self::HistoricalAgentFrameSwitchUnsupported => {
                "historical_agent_frame_switch_unsupported"
            }
            Self::AgentFrameSwitchAuthorConflict => "agent_frame_switch_author_conflict",
            Self::DurableEffectLiveProtocolExtension => "durable_effect_live_protocol_extension",
            Self::DurableEffectLivePluginInput => "durable_effect_live_plugin_input",
            Self::AwaitEventCancelUnsupported => "await_event_cancel_unsupported",
            Self::AwaitEventKeySign => "await_event_key_sign",
            Self::AwaitEventUnknownOrRevoked => "await_event_unknown_or_revoked",
            Self::AwaitEventUnsupported => "await_event_unsupported",
            Self::CancelStartGateUnavailable => "cancel_start_gate_unavailable",
            Self::EffectGroupUnsupported => "effect_group_unsupported",
            Self::EffectJournalRetirementUnsupported => "effect_journal_retirement_unsupported",
            Self::EffectScopeRetired => "effect_scope_retired",
            Self::EffectScopeNotQuiescent => "effect_scope_not_quiescent",
            Self::AwaitEventScopeNotRetirable => "await_event_scope_not_retirable",
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
            Self::ProcessParentEnded => "process_parent_ended",
            Self::ProcessCancelConflict => "process_cancel_conflict",
            Self::DurableIdentityConflict => "durable_identity_conflict",
            Self::ProcessNoLongerRetained => "process_no_longer_retained",
            Self::ProcessIncarnationSuperseded => "process_incarnation_superseded",
            Self::ProcessRegistryUnavailable => "process_registry_unavailable",
            Self::ProcessSignalWaitCancelled => "process_signal_wait_cancelled",
            Self::ProcessSignalWaitTimeout => "process_signal_wait_timeout",
            Self::RestateAwaitEventAwait => "restate_await_event_await",
            Self::RestateAwaitEventCancel => "restate_await_event_cancel",
            Self::RestateAwaitEventPeek => "restate_await_event_peek",
            Self::RestateAwaitEventResolve => "restate_await_event_resolve",
            Self::RestateAwaitEventRevocationRead => "restate_await_event_revocation_read",
            Self::RestateAwaitEventRevoke => "restate_await_event_revoke",
            Self::RestateAwaitEventSessionUpdate => "restate_await_event_session_update",
            Self::RestateEffectController => "restate_effect_controller",
            Self::ToolIntentReplayKeyFormatCutover => "tool_intent_replay_key_format_cutover",
            Self::WorkerReplacementAbort => "worker_replacement_abort",
            Self::RestateJournaledEffectPoisoned => "restate_journaled_effect_poisoned",
            Self::RestateEffectHostRequiresHandlerScope => {
                "restate_effect_host_requires_handler_scope"
            }
            Self::RestateProcessAwait => "restate_process_await",
            Self::RestateProcessCancel => "restate_process_cancel",
            Self::RestateProcessJournalIdentityDrift => "restate_process_journal_identity_drift",
            Self::RestateProcessJournalPayloadIncompatible => {
                "restate_process_journal_payload_incompatible"
            }
            Self::RestateProcessIngressSubmit => "restate_process_ingress_submit",
            Self::RestateServiceUnregistered => "restate_service_unregistered",
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
            Self::RuntimeEffectEnvelopeVersion => "runtime_effect_envelope_version_unsupported",
            Self::RuntimeEffectGroupAwaitCancelled => "runtime_effect_group_await_cancelled",
            Self::RuntimeEffectGroupChildCancelled => "runtime_effect_group_child_cancelled",
            Self::RuntimeEffectGroupChildCancelDecided => {
                "runtime_effect_group_child_cancel_decided"
            }
            Self::RuntimeEffectGroupDrainDeferred => "runtime_effect_group_drain_deferred",
            Self::RuntimeEffectGroupShape => "runtime_effect_group_shape",
            Self::RuntimeEffectInvocationSubject => "runtime_effect_invocation_subject",
            Self::RuntimeEffectScopeMismatch => "runtime_effect_scope_mismatch",
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
            Self::RuntimeEffectToolAttemptCaptureVersion => {
                "runtime_effect_tool_attempt_capture_version"
            }
            Self::RuntimeEffectToolAttemptIndex => "runtime_effect_tool_attempt_index",
            Self::RuntimeEffectToolBatchCallId => "runtime_effect_tool_batch_call_id",
            Self::RuntimeEffectToolBatchCallReplay => "runtime_effect_tool_batch_call_replay",
            Self::RuntimeEffectToolBatchEmpty => "runtime_effect_tool_batch_empty",
            Self::RuntimeEffectToolBatchId => "runtime_effect_tool_batch_id",
            Self::RuntimeEffectToolChildCancellationAuthority => {
                "runtime_effect_tool_child_cancellation_authority"
            }
            Self::RuntimeEffectToolChildCompletionRouting => {
                "runtime_effect_tool_child_completion_routing"
            }
            Self::RuntimeEffectToolChildRequestAdmission => {
                "runtime_effect_tool_child_request_admission"
            }
            Self::RuntimeEffectToolChildRequestCallId => {
                "runtime_effect_tool_child_request_call_id"
            }
            Self::RuntimeEffectToolChildRequestOpener => "runtime_effect_tool_child_request_opener",
            Self::RuntimeEffectToolChildRequestVersion => {
                "runtime_effect_tool_child_request_version"
            }
            Self::RuntimeEffectToolSettlementVersion => "runtime_effect_tool_settlement_version",
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
            self,
            Self::SqliteEffectReplayHashConflict
                | Self::PostgresEffectReplayHashConflict
                | Self::RestateProcessJournalIdentityDrift
                | Self::WorkerReplacementAbort
                | Self::ToolIntentReplayKeyFormatCutover
        )
    }

    /// Whether this error aborts only the in-flight turn because its durable
    /// journal belongs to a replaced worker.
    pub fn is_worker_replacement_abort(&self) -> bool {
        matches!(self, Self::WorkerReplacementAbort)
    }

    /// The decided retry posture of this code.
    ///
    /// This is the single classification site: the match is exhaustive, so a
    /// new variant does not compile until it is deliberately classified.
    /// [`Self::is_retryable`] and [`Self::is_terminal`] are projections of it.
    pub(crate) const fn classification(&self) -> RuntimeErrorClass {
        match self {
            // A hook failure is an incomplete derivation over an already
            // durable completion, so redriving phase 2 is the correct recovery
            // (FIG-1276).
            Self::RuntimeEffectAssistantResponseHook
            | Self::RuntimeEffectGroupDrainDeferred
            | Self::SessionExecutionLaneBusy
            | Self::TurnInputSettlementSuperseded
            | Self::StoreCommitContended
            | Self::CancelStartGateUnavailable
            | Self::PostgresAwaitEventStore
            | Self::PostgresEffectJournalRetirement
            | Self::RestateAwaitEventAwait
            | Self::RestateAwaitEventCancel
            | Self::RestateAwaitEventPeek
            | Self::RestateAwaitEventResolve
            | Self::RestateAwaitEventRevocationRead
            | Self::RestateAwaitEventRevoke
            | Self::RestateAwaitEventSessionUpdate
            | Self::RestateProcessCancel
            | Self::RestateProcessIngressSubmit
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
            | Self::TurnTerminalAwaitTimeout => RuntimeErrorClass::Retryable,
            Self::AttachmentSourcePolicyDenied
            | Self::EffectPanicked
            | Self::MissingExecutionScopeId
            | Self::ExecutionScopeTurnIdMismatch
            | Self::ExecutionScopeAdmissionRefused
            | Self::TurnInputRedriveSetUnavailable
            | Self::TurnExecutionRequiresReconciledToolSurface
            | Self::QueuedWorkRowExceedsContextWindow
            | Self::StoreCommitNodeBudgetExceeded
            | Self::StoreCommitByteBudgetExceeded
            | Self::SessionDeleted
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
            | Self::EffectScopeRetired
            | Self::EffectScopeNotQuiescent
            | Self::AwaitEventScopeNotRetirable
            | Self::InvalidAwaitEventSessionId
            | Self::InvalidAwaitEventWaitIdentity
            | Self::InvalidTurnCancelRequest
            | Self::HistoricalAgentFrameSwitchUnsupported
            | Self::AgentFrameSwitchAuthorConflict
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
            | Self::ToolIntentReplayKeyFormatCutover
            | Self::ProcessPanicked
            | Self::ProcessNotVisible
            | Self::ProcessAlreadyTerminal
            | Self::ProcessParentEnded
            | Self::ProcessCancelConflict
            | Self::DurableIdentityConflict
            | Self::ProcessNoLongerRetained
            | Self::ProcessIncarnationSuperseded
            | Self::ProcessRegistryUnavailable
            | Self::ProcessSignalWaitCancelled
            | Self::ProcessSignalWaitTimeout
            | Self::WorkerReplacementAbort
            | Self::RestateEffectHostRequiresHandlerScope
            | Self::RestateJournaledEffectPoisoned
            | Self::RestateProcessAwait
            | Self::RestateProcessJournalIdentityDrift
            | Self::RestateProcessJournalPayloadIncompatible
            | Self::RestateServiceUnregistered
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
            | Self::RuntimeEffectEnvelopeVersion
            | Self::RuntimeEffectGroupAwaitCancelled
            | Self::RuntimeEffectGroupChildCancelled
            | Self::RuntimeEffectGroupChildCancelDecided
            | Self::RuntimeEffectGroupShape
            | Self::RuntimeEffectInvocationSubject
            | Self::RuntimeEffectScopeMismatch
            | Self::RuntimeEffectLocalExecutorMismatch
            | Self::RuntimeEffectLocalExecutorUnavailable
            | Self::RuntimeEffectLocalTaskClosed
            | Self::RuntimeEffectProcessTaskJoin
            | Self::RuntimeEffectReplayRequired
            | Self::RuntimeEffectSleepCancelled
            | Self::RuntimeEffectTaskJoin
            | Self::RuntimeEffectToolAttemptCallId
            | Self::RuntimeEffectToolAttemptCaptureVersion
            | Self::RuntimeEffectToolAttemptIndex
            | Self::RuntimeEffectToolBatchCallId
            | Self::RuntimeEffectToolBatchCallReplay
            | Self::RuntimeEffectToolBatchEmpty
            | Self::RuntimeEffectToolBatchId
            | Self::RuntimeEffectToolChildCancellationAuthority
            | Self::RuntimeEffectToolChildCompletionRouting
            | Self::RuntimeEffectToolChildRequestAdmission
            | Self::RuntimeEffectToolChildRequestCallId
            | Self::RuntimeEffectToolChildRequestOpener
            | Self::RuntimeEffectToolChildRequestVersion
            | Self::RuntimeEffectToolSettlementVersion
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
            | Self::TriggerStoreUnavailable => RuntimeErrorClass::Terminal,
            Self::SessionExecutionLeaseLost
            | Self::StoreCommitSuperseded
            | Self::ExecutionStateCaptureFailed
            | Self::ResidentSessionReloadFailed
            | Self::StoreCommitFailed
            | Self::PluginSessionManager
            | Self::PluginFinalizeTurn
            | Self::PluginCheckpoint
            | Self::PluginPrepareTurn
            | Self::ContextPrepareTurn
            | Self::ProtocolTurnExtension
            | Self::ProtocolBeforeLlmCall
            | Self::TurnStreamJoin
            | Self::EmptyAgentFrameRun
            | Self::LiveReplay
            | Self::PostgresAwaitEventNotify
            | Self::QueuedWork
            | Self::RuntimeEffectControllerTaskClosed
            | Self::SessionHeadRefresh
            | Self::SqliteAwaitEventNotify
            | Self::TurnControlWaitCancelled
            | Self::ArtifactOwnerRetired
            | Self::ArtifactDestinationOwnerRetired
            | Self::ArtifactStagingEdgeMissing
            | Self::ForeignCode(_) => RuntimeErrorClass::Unclassified,
        }
    }

    /// Whether retrying the identical operation is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        self.classification() == RuntimeErrorClass::Retryable
    }

    /// Whether retrying cannot succeed without changing input, configuration,
    /// wiring, or corrupted durable state.
    pub fn is_terminal(&self) -> bool {
        self.classification() == RuntimeErrorClass::Terminal
    }

    /// Every first-party variant, for test iteration. The variant-count
    /// assertion in `runtime_error_tests` keeps this list complete.
    #[cfg(test)]
    pub(crate) const ALL_FIRST_PARTY: &[Self] = &[
        Self::AttachmentSourcePolicyDenied,
        Self::ArtifactOwnerRetired,
        Self::ArtifactDestinationOwnerRetired,
        Self::ArtifactStagingEdgeMissing,
        Self::EffectPanicked,
        Self::MissingExecutionScopeId,
        Self::ExecutionScopeTurnIdMismatch,
        Self::ExecutionScopeAdmissionRefused,
        Self::SessionExecutionLeaseLost,
        Self::SessionExecutionLaneBusy,
        Self::TurnInputSettlementSuperseded,
        Self::TurnInputRedriveSetUnavailable,
        Self::TurnExecutionRequiresReconciledToolSurface,
        Self::StoreCommitContended,
        Self::StoreCommitSuperseded,
        Self::SessionDeleted,
        Self::StoreCommitNodeBudgetExceeded,
        Self::StoreCommitByteBudgetExceeded,
        Self::CheckpointComponentEncodingVersionMismatch,
        Self::RecordEncodingFailed,
        Self::MissingProcessExecutionId,
        Self::ExecutionStateCaptureFailed,
        Self::ResidentSessionReloadFailed,
        Self::StoreCommitFailed,
        Self::PluginSessionManager,
        Self::PluginFinalizeTurn,
        Self::PluginCheckpoint,
        Self::PluginPrepareTurn,
        Self::ContextPrepareTurn,
        Self::ProtocolTurnExtension,
        Self::ProtocolBeforeLlmCall,
        Self::TurnStreamJoin,
        Self::EmptyAgentFrameRun,
        Self::HistoricalAgentFrameSwitchUnsupported,
        Self::AgentFrameSwitchAuthorConflict,
        Self::DurableEffectLiveProtocolExtension,
        Self::DurableEffectLivePluginInput,
        Self::AwaitEventCancelUnsupported,
        Self::AwaitEventKeySign,
        Self::AwaitEventUnknownOrRevoked,
        Self::AwaitEventUnsupported,
        Self::CancelStartGateUnavailable,
        Self::EffectGroupUnsupported,
        Self::EffectJournalRetirementUnsupported,
        Self::EffectScopeRetired,
        Self::EffectScopeNotQuiescent,
        Self::AwaitEventScopeNotRetirable,
        Self::InvalidAwaitEventSessionId,
        Self::InvalidAwaitEventWaitIdentity,
        Self::InvalidTurnCancelRequest,
        Self::LiveReplay,
        Self::LlmProvider,
        Self::Plugin,
        Self::PostgresEffectReplayCorruptRow,
        Self::PostgresEffectReplayDecode,
        Self::PostgresEffectReplayEncode,
        Self::PostgresEffectReplayHashConflict,
        Self::PostgresEffectReplayKeyMissing,
        Self::PostgresEffectReplayLeaseLost,
        Self::PostgresEffectReplayMissing,
        Self::PostgresEffectReplayStore,
        Self::PostgresAwaitEventDecode,
        Self::PostgresAwaitEventEncode,
        Self::PostgresAwaitEventNotify,
        Self::PostgresAwaitEventSign,
        Self::PostgresAwaitEventStore,
        Self::PostgresEffectJournalRetirement,
        Self::QueuedWork,
        Self::QueuedWorkRowExceedsContextWindow,
        Self::ProcessPanicked,
        Self::ProcessNotVisible,
        Self::ProcessAlreadyTerminal,
        Self::ProcessParentEnded,
        Self::ProcessCancelConflict,
        Self::DurableIdentityConflict,
        Self::ProcessNoLongerRetained,
        Self::ProcessIncarnationSuperseded,
        Self::ProcessRegistryUnavailable,
        Self::ProcessSignalWaitCancelled,
        Self::ProcessSignalWaitTimeout,
        Self::RestateAwaitEventAwait,
        Self::RestateAwaitEventCancel,
        Self::RestateAwaitEventPeek,
        Self::RestateAwaitEventResolve,
        Self::RestateAwaitEventRevocationRead,
        Self::RestateAwaitEventRevoke,
        Self::RestateAwaitEventSessionUpdate,
        Self::RestateEffectController,
        Self::WorkerReplacementAbort,
        Self::ToolIntentReplayKeyFormatCutover,
        Self::RestateEffectHostRequiresHandlerScope,
        Self::RestateJournaledEffectPoisoned,
        Self::RestateProcessAwait,
        Self::RestateProcessCancel,
        Self::RestateProcessJournalIdentityDrift,
        Self::RestateProcessJournalPayloadIncompatible,
        Self::RestateProcessIngressSubmit,
        Self::RestateServiceUnregistered,
        Self::RestateProcessAwaitAfterTurnCancel,
        Self::RestateProcessTurnCancelContextMissing,
        Self::RestateProcessTerminalEncode,
        Self::RestateTurnTerminalAttach,
        Self::RestateTurnTerminalAttachCeilingElapsed,
        Self::RestateTurnTerminalDecode,
        Self::RestateTurnTerminalInvalidResolution,
        Self::RestateTurnCancelScopeMismatch,
        Self::RestateTurnCancelScopeMissing,
        Self::RuntimeEffectAttachmentStore,
        Self::RuntimeEffectEnvelopeCanonicalDecode,
        Self::RuntimeEffectEnvelopeCanonicalHashInvariant,
        Self::RuntimeEffectEnvelopeHash,
        Self::RuntimeEffectEnvelopeVersion,
        Self::RuntimeEffectGroupAwaitCancelled,
        Self::RuntimeEffectGroupChildCancelled,
        Self::RuntimeEffectGroupChildCancelDecided,
        Self::RuntimeEffectGroupDrainDeferred,
        Self::RuntimeEffectGroupShape,
        Self::RuntimeEffectToolChildCancellationAuthority,
        Self::RuntimeEffectToolChildCompletionRouting,
        Self::RuntimeEffectToolChildRequestAdmission,
        Self::RuntimeEffectToolChildRequestCallId,
        Self::RuntimeEffectToolChildRequestOpener,
        Self::RuntimeEffectToolChildRequestVersion,
        Self::RuntimeEffectInvocationSubject,
        Self::RuntimeEffectScopeMismatch,
        Self::RuntimeEffectLocalExecutorMismatch,
        Self::RuntimeEffectLocalExecutorUnavailable,
        Self::RuntimeEffectAssistantResponseHook,
        Self::RuntimeEffectLocalTaskClosed,
        Self::RuntimeEffectProcessTaskJoin,
        Self::RuntimeEffectReplayRequired,
        Self::RuntimeEffectSleepCancelled,
        Self::RuntimeEffectTaskJoin,
        Self::RuntimeEffectToolAttemptCallId,
        Self::RuntimeEffectToolAttemptCaptureVersion,
        Self::RuntimeEffectToolAttemptIndex,
        Self::RuntimeEffectToolBatchCallId,
        Self::RuntimeEffectToolBatchCallReplay,
        Self::RuntimeEffectToolBatchEmpty,
        Self::RuntimeEffectToolBatchId,
        Self::RuntimeEffectToolSettlementVersion,
        Self::RuntimeEffectWrongOutcome,
        Self::RuntimeEffectControllerTaskClosed,
        Self::RuntimePerfStartGateRetry,
        Self::RuntimeStore,
        Self::RuntimeStoreCorrupt,
        Self::SessionCommandClaim,
        Self::SessionCommandIdempotencyKey,
        Self::SessionCommandPostDriveRefresh,
        Self::SessionCommandRefresh,
        Self::SessionCommandRefreshTools,
        Self::SessionDeleteScopeMismatch,
        Self::SessionHeadRefresh,
        Self::SessionToolRegistry,
        Self::SqliteAwaitEventDecode,
        Self::SqliteAwaitEventEncode,
        Self::SqliteAwaitEventNotify,
        Self::SqliteAwaitEventSign,
        Self::SqliteAwaitEventStore,
        Self::SqliteEffectJournalRetirement,
        Self::SqliteEffectReplayCorruptRow,
        Self::SqliteEffectReplayDecode,
        Self::SqliteEffectReplayEncode,
        Self::SqliteEffectReplayHashConflict,
        Self::SqliteEffectReplayKeyMissing,
        Self::SqliteEffectReplayLeaseLost,
        Self::SqliteEffectReplayMissing,
        Self::SqliteEffectReplayStore,
        Self::ToolBatchMissingResult,
        Self::ToolBatchResultCountMismatch,
        Self::ToolCatalogResolutionFailed,
        Self::ToolCompletionKeyMissingCallId,
        Self::ToolCompletionKeyProcessLifetime,
        Self::ToolDeferralNotDeclared,
        Self::TransientCancelWatch,
        Self::TransientTerminalPublication,
        Self::TurnCancelGateDecode,
        Self::TurnCancelGateEncode,
        Self::TurnCancelGateInvalidTerminal,
        Self::TurnControlPeekOutcome,
        Self::TurnControlUnknownOrRevoked,
        Self::TurnControlWaitCancelled,
        Self::TurnControlWaitTimeout,
        Self::TurnTerminalAwaitTimeout,
        Self::TurnTerminalDecode,
        Self::TurnTerminalEncode,
        Self::TurnTerminalInvalidResolution,
        Self::TurnTerminalUnknownOrRevoked,
        Self::TriggerStoreUnavailable,
    ];

    /// Built-in strings are always canonicalized to their dedicated variants;
    /// only unknown extension strings produce [`Self::ForeignCode`]. This is
    /// the supported construction path for host-defined codes.
    pub fn from_wire_code(code: &str) -> Self {
        match code {
            "attachment_source_policy_denied" => Self::AttachmentSourcePolicyDenied,
            "artifact_owner_retired" => Self::ArtifactOwnerRetired,
            "artifact_destination_owner_retired" => Self::ArtifactDestinationOwnerRetired,
            "artifact_staging_edge_missing" => Self::ArtifactStagingEdgeMissing,
            "effect_panicked" => Self::EffectPanicked,
            "missing_execution_scope_id" => Self::MissingExecutionScopeId,
            "execution_scope_turn_id_mismatch" => Self::ExecutionScopeTurnIdMismatch,
            "execution_scope_admission_refused" => Self::ExecutionScopeAdmissionRefused,
            "session_execution_lease_lost" => Self::SessionExecutionLeaseLost,
            "session_execution_lane_busy" => Self::SessionExecutionLaneBusy,
            "turn_input_settlement_superseded" => Self::TurnInputSettlementSuperseded,
            "turn_input_redrive_set_unavailable" => Self::TurnInputRedriveSetUnavailable,
            "turn_execution_requires_reconciled_tool_surface" => {
                Self::TurnExecutionRequiresReconciledToolSurface
            }
            "store_commit_contended" => Self::StoreCommitContended,
            "store_commit_superseded" => Self::StoreCommitSuperseded,
            "session_deleted" => Self::SessionDeleted,
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
            "historical_agent_frame_switch_unsupported" => {
                Self::HistoricalAgentFrameSwitchUnsupported
            }
            "agent_frame_switch_author_conflict" => Self::AgentFrameSwitchAuthorConflict,
            "durable_effect_live_protocol_extension" => Self::DurableEffectLiveProtocolExtension,
            "durable_effect_live_plugin_input" => Self::DurableEffectLivePluginInput,
            "await_event_cancel_unsupported" => Self::AwaitEventCancelUnsupported,
            "await_event_key_sign" => Self::AwaitEventKeySign,
            "await_event_unknown_or_revoked" => Self::AwaitEventUnknownOrRevoked,
            "await_event_unsupported" => Self::AwaitEventUnsupported,
            "cancel_start_gate_unavailable" => Self::CancelStartGateUnavailable,
            "effect_group_unsupported" => Self::EffectGroupUnsupported,
            "effect_journal_retirement_unsupported" => Self::EffectJournalRetirementUnsupported,
            "effect_scope_retired" => Self::EffectScopeRetired,
            "effect_scope_not_quiescent" => Self::EffectScopeNotQuiescent,
            "await_event_scope_not_retirable" => Self::AwaitEventScopeNotRetirable,
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
            "process_parent_ended" => Self::ProcessParentEnded,
            "process_cancel_conflict" => Self::ProcessCancelConflict,
            "durable_identity_conflict" => Self::DurableIdentityConflict,
            "process_no_longer_retained" => Self::ProcessNoLongerRetained,
            "process_incarnation_superseded" => Self::ProcessIncarnationSuperseded,
            "process_registry_unavailable" => Self::ProcessRegistryUnavailable,
            "process_signal_wait_cancelled" => Self::ProcessSignalWaitCancelled,
            "process_signal_wait_timeout" => Self::ProcessSignalWaitTimeout,
            "restate_await_event_await" => Self::RestateAwaitEventAwait,
            "restate_await_event_cancel" => Self::RestateAwaitEventCancel,
            "restate_await_event_peek" => Self::RestateAwaitEventPeek,
            "restate_await_event_resolve" => Self::RestateAwaitEventResolve,
            "restate_await_event_revocation_read" => Self::RestateAwaitEventRevocationRead,
            "restate_await_event_revoke" => Self::RestateAwaitEventRevoke,
            "restate_await_event_session_update" => Self::RestateAwaitEventSessionUpdate,
            "restate_effect_controller" => Self::RestateEffectController,
            "tool_intent_replay_key_format_cutover" => Self::ToolIntentReplayKeyFormatCutover,
            "worker_replacement_abort" | "restate_effect_hash_mismatch" => {
                Self::WorkerReplacementAbort
            }
            "restate_effect_host_requires_handler_scope" => {
                Self::RestateEffectHostRequiresHandlerScope
            }
            "restate_journaled_effect_poisoned" => Self::RestateJournaledEffectPoisoned,
            "restate_process_await" => Self::RestateProcessAwait,
            "restate_process_cancel" => Self::RestateProcessCancel,
            "restate_process_journal_identity_drift" => Self::RestateProcessJournalIdentityDrift,
            "restate_process_journal_payload_incompatible" => {
                Self::RestateProcessJournalPayloadIncompatible
            }
            "restate_process_ingress_submit" => Self::RestateProcessIngressSubmit,
            "restate_service_unregistered" => Self::RestateServiceUnregistered,
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
            "runtime_effect_envelope_version_unsupported" => Self::RuntimeEffectEnvelopeVersion,
            "runtime_effect_group_await_cancelled" => Self::RuntimeEffectGroupAwaitCancelled,
            "runtime_effect_group_child_cancelled" => Self::RuntimeEffectGroupChildCancelled,
            "runtime_effect_group_child_cancel_decided" => {
                Self::RuntimeEffectGroupChildCancelDecided
            }
            "runtime_effect_group_drain_deferred" => Self::RuntimeEffectGroupDrainDeferred,
            "runtime_effect_group_shape" => Self::RuntimeEffectGroupShape,
            "runtime_effect_invocation_subject" => Self::RuntimeEffectInvocationSubject,
            "runtime_effect_scope_mismatch" => Self::RuntimeEffectScopeMismatch,
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
            "runtime_effect_tool_attempt_capture_version" => {
                Self::RuntimeEffectToolAttemptCaptureVersion
            }
            "runtime_effect_tool_attempt_index" => Self::RuntimeEffectToolAttemptIndex,
            "runtime_effect_tool_batch_call_id" => Self::RuntimeEffectToolBatchCallId,
            "runtime_effect_tool_batch_call_replay" => Self::RuntimeEffectToolBatchCallReplay,
            "runtime_effect_tool_batch_empty" => Self::RuntimeEffectToolBatchEmpty,
            "runtime_effect_tool_batch_id" => Self::RuntimeEffectToolBatchId,
            "runtime_effect_tool_child_cancellation_authority" => {
                Self::RuntimeEffectToolChildCancellationAuthority
            }
            "runtime_effect_tool_child_completion_routing" => {
                Self::RuntimeEffectToolChildCompletionRouting
            }
            "runtime_effect_tool_child_request_admission" => {
                Self::RuntimeEffectToolChildRequestAdmission
            }
            "runtime_effect_tool_child_request_call_id" => {
                Self::RuntimeEffectToolChildRequestCallId
            }
            "runtime_effect_tool_child_request_opener" => Self::RuntimeEffectToolChildRequestOpener,
            "runtime_effect_tool_child_request_version" => {
                Self::RuntimeEffectToolChildRequestVersion
            }
            "runtime_effect_tool_settlement_version" => Self::RuntimeEffectToolSettlementVersion,
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
#[non_exhaustive]
pub enum RuntimeErrorCause {
    SessionDeleted { session_id: SessionId },
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
impl From<lash_sansio::EffectIdentityError> for RuntimeError {
    fn from(error: lash_sansio::EffectIdentityError) -> Self {
        let code = match error {
            lash_sansio::EffectIdentityError::MissingExecutionScopeId => {
                RuntimeErrorCode::MissingExecutionScopeId
            }
            lash_sansio::EffectIdentityError::MissingReplayKey => {
                RuntimeErrorCode::RuntimeEffectReplayRequired
            }
        };
        Self::new(code, error.to_string())
    }
}
impl std::error::Error for RuntimeError {}

/// Compact, content-free mismatch evidence retained on the controller error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEffectReplayMismatchReport {
    pub divergent_path_count: usize,
    pub first_divergent_paths: Vec<String>,
}

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    pub code: RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::RuntimeEffectReplayMismatchReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
}

impl RuntimeEffectControllerError {
    pub fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            summary: None,
            cause: None,
        }
    }

    /// Hosts must namespace these codes and must not mint a built-in
    /// [`RuntimeErrorCode`] spelling. First-party producers use [`Self::new`],
    /// whose typed argument makes an unclassified string a compile error.
    pub fn foreign(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorCode::ForeignCode(code.into()), message)
    }

    /// Sets the summary carried by a `RuntimeEffectControllerError` for effect-host implementors
    /// while executing or replaying a runtime effect.
    pub fn with_summary(mut self, summary: crate::RuntimeEffectReplayMismatchReport) -> Self {
        self.summary = Some(summary);
        self
    }

    pub fn wrong_outcome(expected: RuntimeEffectKind, actual: RuntimeEffectKind) -> Self {
        Self::new(
            RuntimeErrorCode::RuntimeEffectWrongOutcome,
            format!(
                "expected {} outcome, got {}",
                expected.as_str(),
                actual.as_str()
            ),
        )
    }

    pub fn into_runtime_error(self) -> RuntimeError {
        let Self {
            code,
            message,
            summary,
            cause,
        } = self;
        let mut runtime = RuntimeError::new(code, message);
        runtime.summary = summary;
        match cause {
            Some(cause) => runtime.with_cause(cause),
            None => runtime,
        }
    }
}

impl From<RuntimeError> for RuntimeEffectControllerError {
    fn from(err: RuntimeError) -> Self {
        Self {
            code: err.code,
            message: err.message,
            summary: err.summary,
            cause: err.cause,
        }
    }
}

impl From<lash_sansio::EffectIdentityError> for RuntimeEffectControllerError {
    fn from(error: lash_sansio::EffectIdentityError) -> Self {
        RuntimeError::from(error).into()
    }
}

impl From<crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: crate::StoreError) -> Self {
        let cause = match &err {
            crate::StoreError::SessionDeleted { session_id } => {
                Some(crate::RuntimeErrorCause::SessionDeleted {
                    session_id: session_id.clone(),
                })
            }
            _ => None,
        };
        let code = match &err {
            crate::StoreError::StoredDataCorrupt { .. }
            | crate::StoreError::MonotonicCounterOverflow { .. } => {
                crate::RuntimeErrorCode::RuntimeStoreCorrupt
            }
            crate::StoreError::SessionDeleted { .. } => crate::RuntimeErrorCode::SessionDeleted,
            crate::StoreError::HeadRevisionConflict { .. } => {
                crate::RuntimeErrorCode::StoreCommitSuperseded
            }
            crate::StoreError::CommitNodeBudgetExceeded { .. } => {
                crate::RuntimeErrorCode::StoreCommitNodeBudgetExceeded
            }
            crate::StoreError::CommitByteBudgetExceeded { .. } => {
                crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded
            }
            crate::StoreError::CheckpointComponentEncodingVersionMismatch { .. } => {
                crate::RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch
            }
            crate::StoreError::RecordEncodingFailed { .. } => {
                crate::RuntimeErrorCode::RecordEncodingFailed
            }
            _ => crate::RuntimeErrorCode::RuntimeStore,
        };
        Self {
            code,
            message: err.to_string(),
            summary: None,
            cause,
        }
    }
}
