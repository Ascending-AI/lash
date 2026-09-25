//! The one classification of a [`RuntimeErrorCode`] (FIG-3575).
//!
//! Every code has exactly one posture, decided here with its reason. The
//! retry projections ([`RuntimeErrorCode::is_retryable`],
//! [`RuntimeErrorCode::is_terminal`]) and the cause a failed turn settles by
//! ([`RuntimeErrorCode::turn_failure_cause`]) are all read from it, so they
//! cannot disagree: a code is terminal exactly when it is an outcome.

use super::RuntimeErrorCode;

/// The decided posture of a [`RuntimeErrorCode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeErrorClass {
    /// Retrying the identical operation is explicitly safe.
    Retryable,
    /// A fact about this attempt: the identical call is not declared safe to
    /// repeat, but a redrive under fresh authority can succeed.
    Redrivable,
    /// Retrying cannot succeed without changing input, configuration,
    /// wiring, or corrupted durable state: a redrive reproduces it.
    Terminal,
    /// A re-executed program refused a replay its journal does not support
    /// (FIG-3586). A redrive by this build reproduces the refusal with zero
    /// dispatch, but the turn is not failed either: redeploying the build
    /// that wrote the journal serves it, so the turn waits for an operator.
    Parked,
}

/// How a turn failure settles, decided by its cause.
///
/// The rule follows FIG-3528: a journaled outcome stays on the result surface,
/// and a live fault aborts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TurnFailureCause {
    /// Deterministic over the turn's journaled inputs: a redrive reproduces it.
    /// A direct turn records it as a failed turn, and a queued run settles
    /// failed once instead of retrying it.
    Outcome,
    /// A fact about this execution attempt, not about the turn: lost lease,
    /// journal or store I/O, or a process-local task, engine or shutdown
    /// fault. Nothing may be recorded, so the invocation aborts with `Err`. An
    /// aborted direct turn returns its acceptance receipt; a queued run stays
    /// pending for its retry budget.
    LiveFault,
    /// A re-executed lashlang run refused to replay a journal it cannot serve
    /// (FIG-3586): its commands no longer match the recorded ones, or the
    /// journal predates this build's key grammar. Nothing was dispatched and
    /// nothing is recorded as the turn's outcome. The invocation aborts with
    /// `Err` exactly as a live fault does — claims held, receipt returned —
    /// and the park is recorded, but a queued run spends no retry budget on
    /// it: every redrive by this build refuses again with zero dispatch, and
    /// what serves the turn is an operator redeploying the build that wrote
    /// its journal, cancelling it, or forking it.
    Parked,
}

impl TurnFailureCause {
    /// Whether a turn failing with this cause aborts its invocation with
    /// `Err` rather than recording a failed turn.
    pub const fn aborts_invocation(self) -> bool {
        matches!(self, Self::LiveFault | Self::Parked)
    }
}

impl RuntimeErrorCode {
    /// The decided posture of this code.
    ///
    /// This is the single classification site: the match is exhaustive, so a
    /// new variant does not compile until it is deliberately classified.
    pub(crate) const fn classification(&self) -> RuntimeErrorClass {
        use RuntimeErrorClass::{Parked, Redrivable, Retryable, Terminal};
        match self {
            // the attachment policy judges the recorded attachment, so it refuses it again.
            Self::AttachmentSourcePolicyDenied => Terminal,
            // a permanent retirement fence already closed the owner.
            Self::ArtifactOwnerRetired => Terminal,
            // a permanent retirement fence already closed the destination owner.
            Self::ArtifactDestinationOwnerRetired => Terminal,
            // neither owner edge exists in durable state; a redrive reads the same state.
            Self::ArtifactStagingEdgeMissing => Terminal,
            // a contained panic of the effect body; the same body panics the same way.
            Self::EffectPanicked => Terminal,
            // the effect names no execution scope; wiring, not the attempt.
            Self::MissingExecutionScopeId => Terminal,
            // the scope names another turn; wiring, not the attempt.
            Self::ExecutionScopeTurnIdMismatch => Terminal,
            // the scope lacks its incarnation; the identical admission fails identically.
            Self::ExecutionScopeAdmissionRefused => Terminal,
            // lease lost: a successor holds the lane, and a redrive under a new lease succeeds.
            Self::SessionExecutionLeaseLost => Redrivable,
            // a live foreign executor holds the lane; a later attempt takes it.
            Self::SessionExecutionLaneBusy => Retryable,
            // the drive lost the head CAS; re-running the turn is how its result is obtained.
            Self::TurnInputSettlementSuperseded => Retryable,
            // the drive is journaled, so re-running the same turn cedes the same way.
            Self::AcceptedTurnInputCeded => Terminal,
            // the open declared it runs no turn; the same open refuses identically.
            Self::TurnExecutionRequiresReconciledToolSurface => Terminal,
            // transactional write authority was contended; the identical commit is safe to retry.
            Self::StoreCommitContended => Retryable,
            // the queued run yielded with a durable continuation that a redrive resumes.
            Self::QueuedRunPending => Retryable,
            // the queued run already settled failed.
            Self::QueuedRunFailed => Terminal,
            // the admitted configuration differs; nothing changes until the host restores or abandons it.
            Self::QueuedRunConfigurationChanged => Terminal,
            // a follow-on owns the session; it runs first, then a redrive finds the head free.
            Self::FollowOnPending => Redrivable,
            // a newer commit moved the head; a redrive reloads it and re-establishes authority.
            Self::StoreCommitSuperseded => Redrivable,
            // the session is gone.
            Self::SessionDeleted => Terminal,
            // a capability fact about the deployment.
            Self::SessionCatalogLookupUnsupported => Terminal,
            // the session's generation marker is older than this build admits; a redrive reads the same marker.
            Self::SessionStateVersionUnsupported => Terminal,
            // the session's generation marker is newer than this build knows; a redrive reads the same marker.
            Self::SessionStateVersionNewerThanRuntime => Terminal,
            // the same turn writes the same nodes and exceeds the budget again.
            Self::StoreCommitNodeBudgetExceeded => Terminal,
            // the same turn writes the same bytes and exceeds the budget again.
            Self::StoreCommitByteBudgetExceeded => Terminal,
            // this build cannot read or write the codec version.
            Self::CheckpointComponentEncodingVersionMismatch => Terminal,
            // serializing the same value with the same build fails the same way.
            Self::RecordEncodingFailed => Terminal,
            // the process execution has no persisted id; wiring, not the attempt.
            Self::MissingProcessExecutionId => Terminal,
            // live executor state could not be captured; nothing was published and a redrive recaptures it.
            Self::ExecutionStateCaptureFailed => Redrivable,
            // resident state is invalidated in this process; a cold reopen from durable state repairs it.
            Self::ResidentSessionReloadFailed => Redrivable,
            // store I/O failed at commit.
            Self::StoreCommitFailed => Redrivable,
            // a session-manager or registry write failed; store I/O, not the turn.
            Self::PluginSessionManager => Redrivable,
            // a plugin finalize hook refused over the same turn.
            Self::PluginFinalizeTurn => Terminal,
            // a plugin checkpoint hook refused over the same turn.
            Self::PluginCheckpoint => Terminal,
            // a plugin prepare hook refused over the same inputs.
            Self::PluginPrepareTurn => Terminal,
            // context preparation over the same inputs fails the same way.
            Self::ContextPrepareTurn => Terminal,
            // the protocol refused the turn extension it was given.
            Self::ProtocolTurnExtension => Terminal,
            // the protocol refused the request before the model call; the same request is refused again.
            Self::ProtocolBeforeLlmCall => Terminal,
            // the facade's turn task died without reporting.
            Self::TurnStreamJoin => Redrivable,
            // the agent-frame run produced no turn; the same run produces none again.
            Self::EmptyAgentFrameRun => Terminal,
            // a historical frame cannot become resident through this API; the switch is refused identically.
            Self::HistoricalAgentFrameSwitchUnsupported => Terminal,
            // two authors named different switches; the identical turn conflicts identically.
            Self::AgentFrameSwitchAuthorConflict => Terminal,
            // a durable effect was handed live protocol state it cannot journal.
            Self::DurableEffectLiveProtocolExtension => Terminal,
            // a durable effect was handed live plugin input it cannot journal.
            Self::DurableEffectLivePluginInput => Terminal,
            // the host does not support await-event cancellation.
            Self::AwaitEventCancelUnsupported => Terminal,
            // signing the same key fails the same way.
            Self::AwaitEventKeySign => Terminal,
            // the await event is unknown or durably revoked.
            Self::AwaitEventUnknownOrRevoked => Terminal,
            // the host does not support await events.
            Self::AwaitEventUnsupported => Terminal,
            // the cancel start gate was unavailable to this attempt; a retry reaches it.
            Self::CancelStartGateUnavailable => Retryable,
            // the host has no effect-group wiring.
            Self::EffectGroupUnsupported => Terminal,
            // the host cannot retire journals.
            Self::EffectJournalRetirementUnsupported => Terminal,
            // the scope is durably retired.
            Self::EffectScopeRetired => Terminal,
            // retirement was asked of a scope with live work; the same request is refused.
            Self::EffectScopeNotQuiescent => Terminal,
            // the group lifecycle is durably pinned.
            Self::EffectGroupLifecyclePinned => Terminal,
            // the scope's await events forbid retirement.
            Self::AwaitEventScopeNotRetirable => Terminal,
            // a malformed session id in the wait identity.
            Self::InvalidAwaitEventSessionId => Terminal,
            // a malformed wait identity.
            Self::InvalidAwaitEventWaitIdentity => Terminal,
            // a malformed cancel request.
            Self::InvalidTurnCancelRequest => Terminal,
            // the process-local live-replay buffer failed; nothing durable is involved.
            Self::LiveReplay => Redrivable,
            // the provider's failure is the recorded model-call result.
            Self::LlmProvider => Terminal,
            // a plugin refusal over the same inputs.
            Self::Plugin => Terminal,
            // the selected queued work cannot be admitted; the same selection is refused again.
            Self::QueuedWork => Terminal,
            // one queued row alone exceeds the window; the same row exceeds it again.
            Self::QueuedWorkRowExceedsContextWindow => Terminal,
            // a contained panic of the process body; the same body panics the same way.
            Self::ProcessPanicked => Terminal,
            // the target process is outside the visible set.
            Self::ProcessNotVisible => Terminal,
            // the target process is durably terminal.
            Self::ProcessAlreadyTerminal => Terminal,
            // the declared parent scope durably ended.
            Self::ProcessParentEnded => Terminal,
            // a different cancellation is durably recorded.
            Self::ProcessCancelConflict => Terminal,
            // the identity is durably bound to different content.
            Self::DurableIdentityConflict => Terminal,
            // the target was replaced by a retention tombstone.
            Self::ProcessNoLongerRetained => Terminal,
            // a newer incarnation durably superseded this one.
            Self::ProcessIncarnationSuperseded => Terminal,
            // no process execution is wired for this call.
            Self::ProcessRegistryUnavailable => Terminal,
            // the durable signal wait settled cancelled.
            Self::ProcessSignalWaitCancelled => Terminal,
            // the durable signal wait settled timed out.
            Self::ProcessSignalWaitTimeout => Terminal,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventAwait => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventCancel => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventPeek => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventResolve => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventRevocationRead => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventRevoke => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineAwaitEventSessionUpdate => Retryable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineEffectController => Redrivable,
            // the redrive diverged from the engine's journal; only the build that wrote it serves it.
            Self::EffectReplayDivergence => Parked,
            // replay met a retired key format; a redrive meets it again.
            Self::ToolIntentReplayKeyFormatCutover => Terminal,
            // the re-executed program no longer issues its recorded commands; only the build that wrote the journal serves it.
            Self::LashlangCellReplayDivergence => Parked,
            // the turn was admitted under another executable generation; only a build of it serves it.
            Self::RetiredGeneration => Parked,
            // a binding the cell's journal names moved; only its recorded results serve it.
            Self::LashlangCellBindingDrift => Parked,
            // the controller cannot answer the frontier read; wiring, not the attempt.
            Self::RecordedJournalReadUnsupported => Terminal,
            // the host runs outside a handler scope; wiring, not the attempt.
            Self::EngineEffectHostRequiresHandlerScope => Terminal,
            // the give-up is journaled, so replay reproduces it.
            Self::EngineJournaledEffectPoisoned => Terminal,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineProcessAwait => Redrivable,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineProcessCancel => Retryable,
            // the live command diverged from its journal entry; a redrive diverges the same way.
            Self::EngineProcessJournalIdentityDrift => Terminal,
            // the journal entry does not decode in this build.
            Self::EngineProcessJournalPayloadIncompatible => Terminal,
            // the index state was written under another protocol version; a
            // redrive meets the same state.
            Self::EngineEffectGroupProtocolRetired => Terminal,
            // engine interaction failed; the engine redrives the invocation.
            Self::EngineProcessIngressSubmit => Retryable,
            // a deployment fact: the service is not bound.
            Self::EngineServiceUnregistered => Terminal,
            // the turn is durably cancelled before the await.
            Self::EngineProcessAwaitAfterTurnCancel => Terminal,
            // the cancel context is not wired.
            Self::EngineProcessTurnCancelContextMissing => Terminal,
            // the same terminal fails to encode again.
            Self::EngineProcessTerminalEncode => Terminal,
            // engine interaction failed; re-attaching is safe.
            Self::EngineTurnTerminalAttach => Retryable,
            // the attach ceiling elapsed; re-attaching is safe.
            Self::EngineTurnTerminalAttachCeilingElapsed => Retryable,
            // the terminal does not decode.
            Self::EngineTurnTerminalDecode => Terminal,
            // the terminal resolution is invalid.
            Self::EngineTurnTerminalInvalidResolution => Terminal,
            // the cancel names another scope; wiring, not the attempt.
            Self::EngineTurnCancelScopeMismatch => Terminal,
            // the cancel names no scope; wiring, not the attempt.
            Self::EngineTurnCancelScopeMissing => Terminal,
            // attachment store I/O failed before the effect ran.
            Self::RuntimeEffectAttachmentStore => Redrivable,
            // the envelope does not decode canonically.
            Self::RuntimeEffectEnvelopeCanonicalDecode => Terminal,
            // the envelope breaks its hash invariant.
            Self::RuntimeEffectEnvelopeCanonicalHashInvariant => Terminal,
            // hashing the same envelope fails the same way.
            Self::RuntimeEffectEnvelopeHash => Terminal,
            // the envelope version is unsupported by this build.
            Self::RuntimeEffectEnvelopeVersion => Terminal,
            // the await was cancelled in this process and its durable rank is untouched for a redrive.
            Self::RuntimeEffectGroupAwaitCancelled => Redrivable,
            // the group's loser disposition durably made the child terminal.
            Self::RuntimeEffectGroupChildCancelled => Terminal,
            // the child's cancel durably won the group's linearization point.
            Self::RuntimeEffectGroupChildCancelDecided => Terminal,
            // the retained invocation expired and is never re-run.
            Self::RuntimeEffectGroupChildAttachExpired => Terminal,
            // this host still works the group; a later drain succeeds.
            Self::RuntimeEffectGroupDrainDeferred => Retryable,
            // the group was assembled inconsistently.
            Self::RuntimeEffectGroupShape => Terminal,
            // an awaited aggregate nothing can settle; the same program awaits it again.
            Self::AggregateAwaitUnsettled => Terminal,
            // the opener's retention bound refuses the same group again.
            Self::EffectGroupOpenerBoundExceeded => Terminal,
            // the child request names the wrong cancellation authority.
            Self::RuntimeEffectToolChildCancellationAuthority => Terminal,
            // the child request routes its completion inconsistently.
            Self::RuntimeEffectToolChildCompletionRouting => Terminal,
            // the child request is refused admission.
            Self::RuntimeEffectToolChildRequestAdmission => Terminal,
            // the child request names an inconsistent call id.
            Self::RuntimeEffectToolChildRequestCallId => Terminal,
            // the child request names an inconsistent opener.
            Self::RuntimeEffectToolChildRequestOpener => Terminal,
            // the child request version is unsupported.
            Self::RuntimeEffectToolChildRequestVersion => Terminal,
            // the invocation names an inconsistent subject.
            Self::RuntimeEffectInvocationSubject => Terminal,
            // the effect names another scope; wiring, not the attempt.
            Self::RuntimeEffectScopeMismatch => Terminal,
            // the local executor does not match the command.
            Self::RuntimeEffectLocalExecutorMismatch => Terminal,
            // no local executor is wired for the command.
            Self::RuntimeEffectLocalExecutorUnavailable => Terminal,
            // phase 2 re-derives over the durable completion, so a redrive repairs it.
            Self::RuntimeEffectAssistantResponseHook => Retryable,
            // the process-local effect task closed.
            Self::RuntimeEffectLocalTaskClosed => Redrivable,
            // the process effect task died without reporting; nothing was recorded.
            Self::RuntimeEffectProcessTaskJoin => Redrivable,
            // the effect carries no replay key; wiring, not the attempt.
            Self::RuntimeEffectReplayRequired => Terminal,
            // the sleep was cancelled in this process; nothing was recorded.
            Self::RuntimeEffectSleepCancelled => Redrivable,
            // the effect task died without reporting; nothing was recorded.
            Self::RuntimeEffectTaskJoin => Redrivable,
            // the attempt names an inconsistent call id.
            Self::RuntimeEffectToolAttemptCallId => Terminal,
            // the attempt capture version is unsupported.
            Self::RuntimeEffectToolAttemptCaptureVersion => Terminal,
            // the attempt index is inconsistent.
            Self::RuntimeEffectToolAttemptIndex => Terminal,
            // the tool settlement version is unsupported.
            Self::RuntimeEffectToolSettlementVersion => Terminal,
            // the effect produced an outcome of the wrong kind for its command.
            Self::RuntimeEffectWrongOutcome => Terminal,
            // the process-local controller task closed; a restart repairs it.
            Self::RuntimeEffectControllerTaskClosed => Redrivable,
            // store I/O failed.
            Self::RuntimeStore => Retryable,
            // durable state is corrupt or a counter is exhausted.
            Self::RuntimeStoreCorrupt => Terminal,
            // the session command claim is refused.
            Self::SessionCommandClaim => Terminal,
            // the idempotency key is bound to a different command.
            Self::SessionCommandIdempotencyKey => Terminal,
            // the post-drive refresh read failed; a retry reads again.
            Self::SessionCommandPostDriveRefresh => Retryable,
            // the refresh read failed; a retry reads again.
            Self::SessionCommandRefresh => Retryable,
            // the tool refresh failed; a retry refreshes again.
            Self::SessionCommandRefreshTools => Retryable,
            // the delete names another scope.
            Self::SessionDeleteScopeMismatch => Terminal,
            // the head refresh read failed; a retry reads again.
            Self::SessionHeadRefresh => Redrivable,
            // the tool registry refuses the same registration.
            Self::SessionToolRegistry => Terminal,
            // the await-event row does not decode.
            Self::SqliteAwaitEventDecode => Terminal,
            // the same value fails to encode again.
            Self::SqliteAwaitEventEncode => Terminal,
            // the process-local notifier failed; a restart repairs it.
            Self::SqliteAwaitEventNotify => Redrivable,
            // signing the same key fails the same way.
            Self::SqliteAwaitEventSign => Terminal,
            // await-event store I/O failed.
            Self::SqliteAwaitEventStore => Retryable,
            // journal retirement store I/O failed; the identical retirement is safe to retry.
            Self::SqliteEffectJournalRetirement => Retryable,
            // the journal row is corrupt; a redrive reads the same row.
            Self::SqliteEffectReplayCorruptRow => Terminal,
            // the journal row does not decode; a redrive reads the same row.
            Self::SqliteEffectReplayDecode => Terminal,
            // the same value fails to encode again.
            Self::SqliteEffectReplayEncode => Terminal,
            // the live run diverged from its journal; a redrive diverges the same way.
            Self::SqliteEffectReplayHashConflict => Parked,
            // the effect carries no replay key; wiring, not the attempt.
            Self::SqliteEffectReplayKeyMissing => Terminal,
            // the journal row lease was lost to another owner.
            Self::SqliteEffectReplayLeaseLost => Redrivable,
            // strict replay found no journal row; a redrive finds none either.
            Self::SqliteEffectReplayMissing => Terminal,
            // journal store I/O failed.
            Self::SqliteEffectReplayStore => Redrivable,
            // the same catalog resolves the same way.
            Self::ToolCatalogResolutionFailed => Terminal,
            // the completion key names no call id.
            Self::ToolCompletionKeyMissingCallId => Terminal,
            // the tool did not declare deferral.
            Self::ToolDeferralNotDeclared => Terminal,
            // the cancel watch failed transiently.
            Self::TransientCancelWatch => Retryable,
            // terminal publication failed transiently.
            Self::TransientTerminalPublication => Retryable,
            // the cancel gate does not decode.
            Self::TurnCancelGateDecode => Terminal,
            // the same gate fails to encode again.
            Self::TurnCancelGateEncode => Terminal,
            // the cancel gate holds an invalid terminal.
            Self::TurnCancelGateInvalidTerminal => Terminal,
            // the peeked turn-control outcome is invalid.
            Self::TurnControlPeekOutcome => Terminal,
            // the turn control is unknown or durably revoked.
            Self::TurnControlUnknownOrRevoked => Terminal,
            // the local observer was cancelled while the durable promise stayed live.
            Self::TurnControlWaitCancelled => Redrivable,
            // the wait timed out in this process; the durable promise is untouched.
            Self::TurnControlWaitTimeout => Retryable,
            // the terminal await timed out in this process.
            Self::TurnTerminalAwaitTimeout => Retryable,
            // the terminal does not decode.
            Self::TurnTerminalDecode => Terminal,
            // the same terminal fails to encode again.
            Self::TurnTerminalEncode => Terminal,
            // the terminal resolution is invalid.
            Self::TurnTerminalInvalidResolution => Terminal,
            // the terminal is unknown or durably revoked.
            Self::TurnTerminalUnknownOrRevoked => Terminal,
            // no trigger store is wired.
            Self::TriggerStoreUnavailable => Terminal,
            // A foreign code is a recorded outcome; a host minting a live
            // fault under it marks the error it builds (FIG-3575).
            Self::ForeignCode(_) => Terminal,
        }
    }

    /// The cause class of a turn that fails with this code: an outcome
    /// exactly when the code is terminal.
    pub const fn turn_failure_cause(&self) -> TurnFailureCause {
        match self.classification() {
            RuntimeErrorClass::Terminal => TurnFailureCause::Outcome,
            RuntimeErrorClass::Retryable | RuntimeErrorClass::Redrivable => {
                TurnFailureCause::LiveFault
            }
            RuntimeErrorClass::Parked => TurnFailureCause::Parked,
        }
    }
}
