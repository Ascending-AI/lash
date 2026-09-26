//! Kernel error vocabulary.
//!
//! `RuntimeError` and its code enum are the typed failure surface every
//! durable operation reports, and they name `StoreError` directly, so they
//! live beside it. The session-facing mapping into `SessionError` stays in
//! `lash-core`.

pub use crate::executable_generation::{ExecutableGeneration, ExecutableGenerationRefusal};
use crate::{RuntimeEffectKind, SessionId};
use serde::{Deserialize, Serialize};

mod classification;
pub(crate) use classification::RuntimeErrorClass;
pub use classification::TurnFailureCause;

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
    /// A claim-less turn-input settlement lost the head CAS to whoever holds or
    /// already settled that row (ADR 0069 §5): no durable record was written.
    /// Since the initial drive set is journaled with its claim (ADR 0069 §6),
    /// no runtime path settles without a claim; the store still verifies the
    /// claim-less predicate, and this code maps its refusal. The FIG-3540
    /// ingress cutover deletes claim-less settlement together with this code.
    TurnInputSettlementSuperseded,
    /// The journaled initial drive of a turn cannot drive the input that turn
    /// accepted: another claim of the live lease generation holds it; it is no
    /// longer open because it was settled, cancelled, or pruned by `vacuum()`;
    /// or, on a replay, a recovery drain reclaimed the journaled rows before the
    /// turn could commit them. Nothing is committed. The drive is a journaled
    /// effect (ADR 0069 §6), so re-running the same turn cedes the same way; the
    /// accepted input is answered, if at all, by the driver that holds or
    /// settled it.
    AcceptedTurnInputCeded,
    /// A caller waited on a session drive (`SessionWorkEngine::await_drive`)
    /// of a deployment that runs no session work: nothing will ever drive the
    /// session, so nothing will answer the wait. Configuring the core with a
    /// session-work engine is the recovery.
    SessionWorkUnavailable,
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
    /// A physical queued attempt yielded with a durable continuation.
    QueuedRunPending,
    /// An exact queued request re-presented a failed terminal receipt.
    QueuedRunFailed,
    /// Restore the admitted configuration or explicitly abandon this run.
    QueuedRunConfigurationChanged,
    /// A pending follow-on owns the session (ADR 0101 §3): the commit or
    /// frame change is refused until the follow-on's own terminal commit.
    FollowOnPending,
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
    /// The configured Session Catalog has no non-creating by-id lookup, so a
    /// consumer that strictly requires that seam can never resolve the
    /// session it names. A capability fact about the deployment, not a
    /// transient miss: retrying the identical lookup cannot change the
    /// answer, so this is terminal.
    SessionCatalogLookupUnsupported,
    /// The session's durable state is an older generation than this build
    /// admits (FIG-3571, FIG-3619). Under the clean-cutover policy it is
    /// refused before any turn, model, tool or provider effect. A redrive on
    /// this build reads the same marker, so the refusal is terminal. The
    /// message names both generations; the error a refused call returns also
    /// carries them typed in
    /// [`RuntimeError::session_state_version_refusal`].
    SessionStateVersionUnsupported,
    /// The session's durable state is a newer generation than this build
    /// knows. Only a build that admits that generation can run it; the
    /// generations are carried as for
    /// [`Self::SessionStateVersionUnsupported`].
    SessionStateVersionNewerThanRuntime,
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
    EffectGroupLifecyclePinned,
    AwaitEventScopeNotRetirable,
    InvalidAwaitEventSessionId,
    InvalidAwaitEventWaitIdentity,
    InvalidTurnCancelRequest,
    LiveReplay,
    LlmProvider,
    /// A config command named a route the host's provider resolver does not
    /// serve (FIG-3600 S6): refused at send, or at apply, typed.
    ProviderRouteUnknown,
    /// A config command named a route the host's resolver knows but holds no
    /// credentials for (FIG-3600 S6): refused typed, like an unknown route.
    ProviderCredentialsMissing,
    /// The route a turn recorded at its start cannot be bound to a provider
    /// on this worker (FIG-3600 S6, D3 Q3). The route was validated when it
    /// was set, so this is the worker's deployment, not the session's intent:
    /// the engine retries the root, and its retry budget parks it.
    ProviderBindingUnavailable,
    Plugin,
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
    /// tool-intent front door (FIG-1489), and at turn-input admission when a
    /// source key is re-presented with a submission whose digest differs from
    /// the one the row was admitted under (FIG-3544).
    DurableIdentityConflict,
    /// ADR 0051 effect-host implementor diagnostic for a process-command
    /// refusal whose terminal target has been replaced by a retention tombstone.
    ProcessNoLongerRetained,
    ProcessRegistryUnavailable,
    ProcessSignalWaitCancelled,
    ProcessSignalWaitTimeout,
    EngineAwaitEventAwait,
    EngineAwaitEventCancel,
    EngineAwaitEventPeek,
    EngineAwaitEventResolve,
    EngineAwaitEventRevocationRead,
    EngineAwaitEventRevoke,
    EngineAwaitEventSessionUpdate,
    EngineEffectController,
    /// Replay found a retired tool-intent v1 key; re-execution under v2 could
    /// duplicate or diverge from the committed command, so it is refused.
    ToolIntentReplayKeyFormatCutover,
    /// A re-executed lashlang run — a code cell or a process body — issued a
    /// command that is not the one its journal recorded at that issue
    /// ordinal, or issued one while the journal still held entries at or
    /// beyond it (FIG-3586). Nothing was dispatched; the run stopped and the
    /// turn parks until an operator redeploys the build that wrote the
    /// journal, cancels, or forks.
    LashlangCellReplayDivergence,
    /// A turn was redriven under another executable generation than the one
    /// its admission recorded (FIG-3571): the build running the redrive would
    /// compile, key or meter its cells differently from the build that wrote
    /// its journal, so it is refused at admission, before any effect, and the
    /// turn parks for a build of its own generation.
    RetiredGeneration,
    /// A redriven code cell needed a host tool binding its journaled binding
    /// set names, and the live tool for it is now missing or changed
    /// (FIG-3587). The binding is served only from recorded results: a call
    /// that would reach the live tool refuses, before anything is claimed.
    LashlangCellBindingDrift,
    /// A durable effect controller that does not answer the recorded-frontier
    /// read was asked for it: a replayed lashlang run cannot know which of its
    /// commands the journal holds, so it refuses to run rather than dispatch
    /// blind (FIG-3586).
    RecordedJournalReadUnsupported,
    /// A redriven effect's reconstructed envelope, or an effect group's
    /// reopened shape, differs from the one its engine journal recorded. The
    /// engine-neutral divergence code for journals the engine owns (the SQL
    /// hosts keep their store-qualified hash-conflict codes). Nothing was
    /// dispatched; the turn parks and the engine keeps the journal until an
    /// operator redeploys the build that wrote it, cancels, or forks.
    EffectReplayDivergence,
    EngineEffectHostRequiresHandlerScope,
    /// A journaled Restate effect produced an unacceptable outcome and became
    /// terminal rather than failing every enclosing-turn redrive.
    EngineJournaledEffectPoisoned,
    EngineProcessAwait,
    EngineProcessCancel,
    /// A Restate DirectProcess redrive addressed an existing journal entry
    /// with a different canonical process-command identity.
    EngineProcessJournalIdentityDrift,
    /// A Restate DirectProcess journal entry has an unsupported version or a
    /// shape this build cannot decode exactly.
    EngineProcessJournalPayloadIncompatible,
    /// A Restate object's retained state carries a stored-format stamp this
    /// build does not read — unstamped pre-format state, or a newer or
    /// skipped format; the handler refuses the value before any effect.
    EngineObjectStateFormatUnsupported,
    EngineProcessIngressSubmit,
    /// The ingress target names an unbound service; retry cannot change that
    /// deployment fact, so this code is terminal.
    EngineServiceUnregistered,
    EngineProcessAwaitAfterTurnCancel,
    EngineProcessTurnCancelContextMissing,
    EngineProcessTerminalEncode,
    EngineTurnTerminalAttach,
    /// A Restate terminal attachment elapsed; re-attaching is safe.
    EngineTurnTerminalAttachCeilingElapsed,
    EngineTurnTerminalDecode,
    EngineTurnTerminalInvalidResolution,
    EngineTurnCancelScopeMismatch,
    EngineTurnCancelScopeMissing,
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
    /// A successor attaching a retained child invocation id found the
    /// original's retention expired; the retained invocation is gone and the
    /// child is never re-run under a fresh identity (ADR 0099 §8).
    RuntimeEffectGroupChildAttachExpired,
    /// Drain deferred while this host still works the group or its children.
    /// Retry succeeds once it finishes; permanent refusal uses
    /// `RuntimeEffectGroupShape`.
    RuntimeEffectGroupDrainDeferred,
    /// A durable effect group was assembled with children that disagree with the
    /// group they claim to belong to, or an effect carrying group membership
    /// reached a command shape that cannot honor it.
    RuntimeEffectGroupShape,
    /// An awaited aggregate nothing can ever settle — `Promise.race([])`.
    /// ECMA-262 leaves such a promise pending forever; the host ends the
    /// execution with this typed failure instead of parking it, the analogue
    /// of Node exiting on an unsettled top-level await (ADR 0099 §11 clause 5,
    /// ADR 0062). A host lifetime contract, not a catchable exception.
    AggregateAwaitUnsettled,
    /// Opening an effect group would take its logical opener past the work it
    /// may retain at once (ADR 0099 §9). Refused whole, before any child is
    /// dispatched; a group already accepted is never refused this way.
    EffectGroupOpenerBoundExceeded,
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
    ToolCatalogResolutionFailed,
    ToolCompletionKeyMissingCallId,
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
    /// The code is a recorded outcome, terminal (FIG-3575). A host that mints
    /// a live fault under a foreign code says so on the error it builds
    /// ([`RuntimeError::foreign`], [`RuntimeEffectControllerError::foreign`]);
    /// the class is not part of the wire spelling.
    #[non_exhaustive]
    ForeignCode(String),
}
/// Map a turn-input admission failure to the host's error vocabulary.
///
/// A source-key conflict is the host's own contract violation (the same key
/// with a different submission), so it surfaces typed as
/// [`RuntimeErrorCode::DurableIdentityConflict`]; every other admission failure
/// is a store commit failure.
pub fn runtime_error_from_turn_input_admission(err: crate::store::StoreError) -> RuntimeError {
    match err {
        err @ (crate::store::StoreError::PendingTurnInputSourceKeyConflict { .. }
        | crate::store::StoreError::PendingTurnInputIdConflict { .. }) => {
            RuntimeError::new(RuntimeErrorCode::DurableIdentityConflict, err.to_string())
        }
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
}

pub fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    match err {
        err @ (crate::store::StoreError::PendingTurnInputSourceKeyConflict { .. }
        | crate::store::StoreError::PendingTurnInputIdConflict { .. }) => {
            runtime_error_from_turn_input_admission(err)
        }
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
        // A closing session is being deleted: its CloseSession intent is the
        // point of no return, so to a caller it is already gone.
        ref err @ crate::store::StoreError::SessionClosing { ref session_id, .. } => {
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
        ref err @ crate::store::StoreError::SessionStateVersionUnsupported { found, current } => {
            RuntimeError::new(
                RuntimeErrorCode::SessionStateVersionUnsupported,
                err.to_string(),
            )
            .with_session_state_version_refusal(SessionStateVersionRefusal { found, current })
        }
        ref err @ crate::store::StoreError::SessionStateVersionNewerThanRuntime {
            found,
            current,
        } => RuntimeError::new(
            RuntimeErrorCode::SessionStateVersionNewerThanRuntime,
            err.to_string(),
        )
        .with_session_state_version_refusal(SessionStateVersionRefusal { found, current }),
        ref err @ (crate::store::StoreError::FollowOnPending { .. }
        | crate::store::StoreError::FollowOnFrameNotCurrent { .. }
        | crate::store::StoreError::FollowOnNotPending { .. }) => {
            RuntimeError::new(RuntimeErrorCode::FollowOnPending, err.to_string())
        }
        crate::store::StoreError::QueuedRunConfigurationChanged { session_id } => {
            RuntimeError::new(
                RuntimeErrorCode::QueuedRunConfigurationChanged,
                format!(
                    "session {session_id} has a pending queued run with different execution configuration; restore that configuration or explicitly abandon the admission"
                ),
            )
        }
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
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
            Self::AcceptedTurnInputCeded => "accepted_turn_input_ceded",
            Self::SessionWorkUnavailable => "session_work_unavailable",
            Self::TurnExecutionRequiresReconciledToolSurface => {
                "turn_execution_requires_reconciled_tool_surface"
            }
            Self::StoreCommitContended => "store_commit_contended",
            Self::QueuedRunPending => "queued_run_pending",
            Self::QueuedRunFailed => "queued_run_failed",
            Self::QueuedRunConfigurationChanged => "queued_run_configuration_changed",
            Self::FollowOnPending => "follow_on_pending",
            Self::StoreCommitSuperseded => "store_commit_superseded",
            Self::SessionDeleted => "session_deleted",
            Self::SessionCatalogLookupUnsupported => "session_catalog_lookup_unsupported",
            Self::SessionStateVersionUnsupported => "session_state_version_unsupported",
            Self::SessionStateVersionNewerThanRuntime => "session_state_version_newer_than_runtime",
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
            Self::EffectGroupLifecyclePinned => "effect_group_lifecycle_pinned",
            Self::AwaitEventScopeNotRetirable => "await_event_scope_not_retirable",
            Self::InvalidAwaitEventSessionId => "invalid_await_event_session_id",
            Self::InvalidAwaitEventWaitIdentity => "invalid_await_event_wait_identity",
            Self::InvalidTurnCancelRequest => "invalid_turn_cancel_request",
            Self::LiveReplay => "live_replay",
            Self::LlmProvider => "llm_provider",
            Self::ProviderRouteUnknown => "provider_route_unknown",
            Self::ProviderCredentialsMissing => "provider_credentials_missing",
            Self::ProviderBindingUnavailable => "provider_binding_unavailable",
            Self::Plugin => "plugin",
            Self::QueuedWork => "queued_work",
            Self::QueuedWorkRowExceedsContextWindow => "queued_work_row_exceeds_context_window",
            Self::ProcessPanicked => "process_panicked",
            Self::ProcessNotVisible => "process_not_visible",
            Self::ProcessAlreadyTerminal => "process_already_terminal",
            Self::ProcessParentEnded => "process_parent_ended",
            Self::ProcessCancelConflict => "process_cancel_conflict",
            Self::DurableIdentityConflict => "durable_identity_conflict",
            Self::ProcessNoLongerRetained => "process_no_longer_retained",
            Self::ProcessRegistryUnavailable => "process_registry_unavailable",
            Self::ProcessSignalWaitCancelled => "process_signal_wait_cancelled",
            Self::ProcessSignalWaitTimeout => "process_signal_wait_timeout",
            Self::EngineAwaitEventAwait => "engine_await_event_await",
            Self::EngineAwaitEventCancel => "engine_await_event_cancel",
            Self::EngineAwaitEventPeek => "engine_await_event_peek",
            Self::EngineAwaitEventResolve => "engine_await_event_resolve",
            Self::EngineAwaitEventRevocationRead => "engine_await_event_revocation_read",
            Self::EngineAwaitEventRevoke => "engine_await_event_revoke",
            Self::EngineAwaitEventSessionUpdate => "engine_await_event_session_update",
            Self::EngineEffectController => "engine_effect_controller",
            Self::ToolIntentReplayKeyFormatCutover => "tool_intent_replay_key_format_cutover",
            Self::LashlangCellReplayDivergence => "lashlang_cell_replay_divergence",
            Self::RetiredGeneration => "retired_generation",
            Self::LashlangCellBindingDrift => "lashlang_cell_binding_drift",
            Self::RecordedJournalReadUnsupported => "recorded_journal_read_unsupported",
            Self::EffectReplayDivergence => "effect_replay_divergence",
            Self::EngineJournaledEffectPoisoned => "engine_journaled_effect_poisoned",
            Self::EngineEffectHostRequiresHandlerScope => {
                "engine_effect_host_requires_handler_scope"
            }
            Self::EngineProcessAwait => "engine_process_await",
            Self::EngineProcessCancel => "engine_process_cancel",
            Self::EngineProcessJournalIdentityDrift => "engine_process_journal_identity_drift",
            Self::EngineProcessJournalPayloadIncompatible => {
                "engine_process_journal_payload_incompatible"
            }
            Self::EngineObjectStateFormatUnsupported => "engine_object_state_format_unsupported",
            Self::EngineProcessIngressSubmit => "engine_process_ingress_submit",
            Self::EngineServiceUnregistered => "engine_service_unregistered",
            Self::EngineProcessAwaitAfterTurnCancel => "engine_process_await_after_turn_cancel",
            Self::EngineProcessTurnCancelContextMissing => {
                "engine_process_turn_cancel_context_missing"
            }
            Self::EngineProcessTerminalEncode => "engine_process_terminal_encode",
            Self::EngineTurnTerminalAttach => "engine_turn_terminal_attach",
            Self::EngineTurnTerminalAttachCeilingElapsed => {
                "engine_turn_terminal_attach_ceiling_elapsed"
            }
            Self::EngineTurnTerminalDecode => "engine_turn_terminal_decode",
            Self::EngineTurnTerminalInvalidResolution => "engine_turn_terminal_invalid_resolution",
            Self::EngineTurnCancelScopeMismatch => "engine_turn_cancel_scope_mismatch",
            Self::EngineTurnCancelScopeMissing => "engine_turn_cancel_scope_missing",
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
            Self::RuntimeEffectGroupChildAttachExpired => {
                "runtime_effect_group_child_attach_expired"
            }
            Self::RuntimeEffectGroupDrainDeferred => "runtime_effect_group_drain_deferred",
            Self::RuntimeEffectGroupShape => "runtime_effect_group_shape",
            Self::AggregateAwaitUnsettled => "aggregate_await_unsettled",
            Self::EffectGroupOpenerBoundExceeded => "effect_group_opener_bound_exceeded",
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
            Self::ToolCatalogResolutionFailed => "tool_catalog_resolution_failed",
            Self::ToolCompletionKeyMissingCallId => "tool_completion_key_missing_call_id",
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
                | Self::EngineProcessJournalIdentityDrift
                | Self::EffectReplayDivergence
                | Self::ToolIntentReplayKeyFormatCutover
                | Self::LashlangCellReplayDivergence
                | Self::RetiredGeneration
                | Self::LashlangCellBindingDrift
        )
    }

    /// Whether this code parks the turn it fails (FIG-3586): a re-executed
    /// lashlang run refused a replay it cannot serve, nothing was dispatched,
    /// and the turn neither fails nor retries live — its claims stay held and
    /// it waits for an operator.
    pub fn parks_turn(&self) -> bool {
        self.turn_failure_cause() == TurnFailureCause::Parked
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
        Self::AcceptedTurnInputCeded,
        Self::SessionWorkUnavailable,
        Self::TurnExecutionRequiresReconciledToolSurface,
        Self::StoreCommitContended,
        Self::QueuedRunPending,
        Self::QueuedRunFailed,
        Self::QueuedRunConfigurationChanged,
        Self::FollowOnPending,
        Self::StoreCommitSuperseded,
        Self::SessionDeleted,
        Self::SessionCatalogLookupUnsupported,
        Self::SessionStateVersionUnsupported,
        Self::SessionStateVersionNewerThanRuntime,
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
        Self::EffectGroupLifecyclePinned,
        Self::AwaitEventScopeNotRetirable,
        Self::InvalidAwaitEventSessionId,
        Self::InvalidAwaitEventWaitIdentity,
        Self::InvalidTurnCancelRequest,
        Self::LiveReplay,
        Self::LlmProvider,
        Self::ProviderRouteUnknown,
        Self::ProviderCredentialsMissing,
        Self::ProviderBindingUnavailable,
        Self::Plugin,
        Self::QueuedWork,
        Self::QueuedWorkRowExceedsContextWindow,
        Self::ProcessPanicked,
        Self::ProcessNotVisible,
        Self::ProcessAlreadyTerminal,
        Self::ProcessParentEnded,
        Self::ProcessCancelConflict,
        Self::DurableIdentityConflict,
        Self::ProcessNoLongerRetained,
        Self::ProcessRegistryUnavailable,
        Self::ProcessSignalWaitCancelled,
        Self::ProcessSignalWaitTimeout,
        Self::EngineAwaitEventAwait,
        Self::EngineAwaitEventCancel,
        Self::EngineAwaitEventPeek,
        Self::EngineAwaitEventResolve,
        Self::EngineAwaitEventRevocationRead,
        Self::EngineAwaitEventRevoke,
        Self::EngineAwaitEventSessionUpdate,
        Self::EngineEffectController,
        Self::EffectReplayDivergence,
        Self::ToolIntentReplayKeyFormatCutover,
        Self::LashlangCellReplayDivergence,
        Self::RetiredGeneration,
        Self::LashlangCellBindingDrift,
        Self::RecordedJournalReadUnsupported,
        Self::EngineEffectHostRequiresHandlerScope,
        Self::EngineJournaledEffectPoisoned,
        Self::EngineProcessAwait,
        Self::EngineProcessCancel,
        Self::EngineProcessJournalIdentityDrift,
        Self::EngineProcessJournalPayloadIncompatible,
        Self::EngineObjectStateFormatUnsupported,
        Self::EngineProcessIngressSubmit,
        Self::EngineServiceUnregistered,
        Self::EngineProcessAwaitAfterTurnCancel,
        Self::EngineProcessTurnCancelContextMissing,
        Self::EngineProcessTerminalEncode,
        Self::EngineTurnTerminalAttach,
        Self::EngineTurnTerminalAttachCeilingElapsed,
        Self::EngineTurnTerminalDecode,
        Self::EngineTurnTerminalInvalidResolution,
        Self::EngineTurnCancelScopeMismatch,
        Self::EngineTurnCancelScopeMissing,
        Self::RuntimeEffectAttachmentStore,
        Self::RuntimeEffectEnvelopeCanonicalDecode,
        Self::RuntimeEffectEnvelopeCanonicalHashInvariant,
        Self::RuntimeEffectEnvelopeHash,
        Self::RuntimeEffectEnvelopeVersion,
        Self::RuntimeEffectGroupAwaitCancelled,
        Self::RuntimeEffectGroupChildCancelled,
        Self::RuntimeEffectGroupChildCancelDecided,
        Self::RuntimeEffectGroupChildAttachExpired,
        Self::RuntimeEffectGroupDrainDeferred,
        Self::RuntimeEffectGroupShape,
        Self::AggregateAwaitUnsettled,
        Self::EffectGroupOpenerBoundExceeded,
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
        Self::RuntimeEffectToolSettlementVersion,
        Self::RuntimeEffectWrongOutcome,
        Self::RuntimeEffectControllerTaskClosed,
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
        Self::ToolCatalogResolutionFailed,
        Self::ToolCompletionKeyMissingCallId,
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
    /// only unknown extension strings produce [`Self::ForeignCode`], as a
    /// recorded outcome. A host minting a live fault under its own code builds
    /// the error with [`RuntimeError::foreign`], which takes its cause class.
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
            "accepted_turn_input_ceded" => Self::AcceptedTurnInputCeded,
            "session_work_unavailable" => Self::SessionWorkUnavailable,
            "turn_execution_requires_reconciled_tool_surface" => {
                Self::TurnExecutionRequiresReconciledToolSurface
            }
            "store_commit_contended" => Self::StoreCommitContended,
            "queued_run_pending" => Self::QueuedRunPending,
            "queued_run_failed" => Self::QueuedRunFailed,
            "queued_run_configuration_changed" => Self::QueuedRunConfigurationChanged,
            "follow_on_pending" => Self::FollowOnPending,
            "store_commit_superseded" => Self::StoreCommitSuperseded,
            "session_deleted" => Self::SessionDeleted,
            "session_catalog_lookup_unsupported" => Self::SessionCatalogLookupUnsupported,
            "session_state_version_unsupported" => Self::SessionStateVersionUnsupported,
            "session_state_version_newer_than_runtime" => Self::SessionStateVersionNewerThanRuntime,
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
            "effect_group_lifecycle_pinned" => Self::EffectGroupLifecyclePinned,
            "await_event_scope_not_retirable" => Self::AwaitEventScopeNotRetirable,
            "invalid_await_event_session_id" => Self::InvalidAwaitEventSessionId,
            "invalid_await_event_wait_identity" => Self::InvalidAwaitEventWaitIdentity,
            "invalid_turn_cancel_request" => Self::InvalidTurnCancelRequest,
            "live_replay" => Self::LiveReplay,
            "llm_provider" => Self::LlmProvider,
            "provider_route_unknown" => Self::ProviderRouteUnknown,
            "provider_credentials_missing" => Self::ProviderCredentialsMissing,
            "provider_binding_unavailable" => Self::ProviderBindingUnavailable,
            "plugin" => Self::Plugin,
            "queued_work" => Self::QueuedWork,
            "queued_work_row_exceeds_context_window" => Self::QueuedWorkRowExceedsContextWindow,
            "process_panicked" => Self::ProcessPanicked,
            "process_not_visible" => Self::ProcessNotVisible,
            "process_already_terminal" => Self::ProcessAlreadyTerminal,
            "process_parent_ended" => Self::ProcessParentEnded,
            "process_cancel_conflict" => Self::ProcessCancelConflict,
            "durable_identity_conflict" => Self::DurableIdentityConflict,
            "process_no_longer_retained" => Self::ProcessNoLongerRetained,
            "process_registry_unavailable" => Self::ProcessRegistryUnavailable,
            "process_signal_wait_cancelled" => Self::ProcessSignalWaitCancelled,
            "process_signal_wait_timeout" => Self::ProcessSignalWaitTimeout,
            "engine_await_event_await" => Self::EngineAwaitEventAwait,
            "engine_await_event_cancel" => Self::EngineAwaitEventCancel,
            "engine_await_event_peek" => Self::EngineAwaitEventPeek,
            "engine_await_event_resolve" => Self::EngineAwaitEventResolve,
            "engine_await_event_revocation_read" => Self::EngineAwaitEventRevocationRead,
            "engine_await_event_revoke" => Self::EngineAwaitEventRevoke,
            "engine_await_event_session_update" => Self::EngineAwaitEventSessionUpdate,
            "engine_effect_controller" => Self::EngineEffectController,
            "tool_intent_replay_key_format_cutover" => Self::ToolIntentReplayKeyFormatCutover,
            "lashlang_cell_replay_divergence" => Self::LashlangCellReplayDivergence,
            "retired_generation" => Self::RetiredGeneration,
            "lashlang_cell_binding_drift" => Self::LashlangCellBindingDrift,
            "recorded_journal_read_unsupported" => Self::RecordedJournalReadUnsupported,
            "effect_replay_divergence" => Self::EffectReplayDivergence,
            "engine_effect_host_requires_handler_scope" => {
                Self::EngineEffectHostRequiresHandlerScope
            }
            "engine_journaled_effect_poisoned" => Self::EngineJournaledEffectPoisoned,
            "engine_process_await" => Self::EngineProcessAwait,
            "engine_process_cancel" => Self::EngineProcessCancel,
            "engine_process_journal_identity_drift" => Self::EngineProcessJournalIdentityDrift,
            "engine_process_journal_payload_incompatible" => {
                Self::EngineProcessJournalPayloadIncompatible
            }
            "engine_object_state_format_unsupported" => Self::EngineObjectStateFormatUnsupported,
            "engine_process_ingress_submit" => Self::EngineProcessIngressSubmit,
            "engine_service_unregistered" => Self::EngineServiceUnregistered,
            "engine_process_await_after_turn_cancel" => Self::EngineProcessAwaitAfterTurnCancel,
            "engine_process_turn_cancel_context_missing" => {
                Self::EngineProcessTurnCancelContextMissing
            }
            "engine_process_terminal_encode" => Self::EngineProcessTerminalEncode,
            "engine_turn_terminal_attach" => Self::EngineTurnTerminalAttach,
            "engine_turn_terminal_attach_ceiling_elapsed" => {
                Self::EngineTurnTerminalAttachCeilingElapsed
            }
            "engine_turn_terminal_decode" => Self::EngineTurnTerminalDecode,
            "engine_turn_terminal_invalid_resolution" => Self::EngineTurnTerminalInvalidResolution,
            "engine_turn_cancel_scope_mismatch" => Self::EngineTurnCancelScopeMismatch,
            "engine_turn_cancel_scope_missing" => Self::EngineTurnCancelScopeMissing,
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
            "runtime_effect_group_child_attach_expired" => {
                Self::RuntimeEffectGroupChildAttachExpired
            }
            "runtime_effect_group_drain_deferred" => Self::RuntimeEffectGroupDrainDeferred,
            "runtime_effect_group_shape" => Self::RuntimeEffectGroupShape,
            "aggregate_await_unsettled" => Self::AggregateAwaitUnsettled,
            "effect_group_opener_bound_exceeded" => Self::EffectGroupOpenerBoundExceeded,
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
            "tool_catalog_resolution_failed" => Self::ToolCatalogResolutionFailed,
            "tool_completion_key_missing_call_id" => Self::ToolCompletionKeyMissingCallId,
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

    /// The namespaced spelling an extension minted, when this is
    /// [`Self::ForeignCode`].
    pub fn foreign_code(&self) -> Option<&str> {
        match self {
            Self::ForeignCode(code) => Some(code.as_str()),
            _ => None,
        }
    }
}

impl From<&RuntimeErrorCode> for lash_sansio::FailureCode {
    /// Maps a runtime error code onto a namespaced failure code: built-in
    /// spellings are workspace vocabulary and land in `lash`, while a
    /// [`RuntimeErrorCode::ForeignCode`] decodes through the foreign-ingress
    /// path — a genuine foreign pair keeps its namespace verbatim, and a
    /// foreign value claiming a reserved namespace or carrying no namespace
    /// lands in `foreign`, never re-minted as a `lash` code.
    fn from(code: &RuntimeErrorCode) -> Self {
        match code.foreign_code() {
            Some(foreign) => lash_sansio::FailureCode::from_foreign_wire(foreign),
            None => lash_sansio::FailureCode::lash(lash_sansio::TurnFailureCode::from_wire(
                code.as_str(),
            )),
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

/// The session-state generations an admission refused (FIG-3619): the one
/// the session's marker holds and the one this build admits.
///
/// In-process only. A [`RuntimeError`] carries it to the host whose call was
/// refused and never serializes it, so no reader of a stored error meets a
/// shape an older build cannot decode. A stored error keeps the code
/// ([`RuntimeErrorCode::SessionStateVersionUnsupported`] or
/// [`RuntimeErrorCode::SessionStateVersionNewerThanRuntime`]) and a message
/// naming both generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStateVersionRefusal {
    pub found: u32,
    pub current: u32,
}

impl SessionStateVersionRefusal {
    /// The generations `error` refused, when it is the generation gate's
    /// refusal.
    #[must_use]
    pub fn of_store_error(error: &crate::store::StoreError) -> Option<Self> {
        match *error {
            crate::store::StoreError::SessionStateVersionUnsupported { found, current }
            | crate::store::StoreError::SessionStateVersionNewerThanRuntime { found, current } => {
                Some(Self { found, current })
            }
            _ => None,
        }
    }
}
/// Runtime error for unexpected failures.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct RuntimeError {
    pub code: RuntimeErrorCode,
    pub message: String,
    /// Structured, content-free evidence for a replay mismatch. Boxed, like
    /// the acceptance below, so the rare diagnostic does not size every
    /// runtime error inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Box<crate::RuntimeEffectReplayMismatchReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RuntimeErrorCause>,
    /// The acceptance of the direct turn this error aborted (FIG-3575).
    ///
    /// Present when a direct turn aborts after its input was durably
    /// accepted: the host names the input by this receipt to withdraw it, or
    /// redrives the same turn. Until then the input is bound to the aborted
    /// turn, and no other turn or drain claims it (FIG-3589).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_acceptance:
        Option<Box<crate::turn_input_vocabulary::TurnInputAcceptanceReceipt>>,
    /// The cause class the minting host chose for a foreign code (FIG-3575).
    /// Never persisted: read back from the wire, a foreign code is a recorded
    /// outcome.
    #[serde(skip)]
    foreign_cause: Option<TurnFailureCause>,
    /// The generations a session-state admission refused (FIG-3619). Never
    /// persisted; see [`SessionStateVersionRefusal`].
    #[serde(skip)]
    session_state_version_refusal: Option<SessionStateVersionRefusal>,
    /// The generations an executable-generation admission refused (FIG-3571).
    /// Never persisted, like the session-state refusal above.
    #[serde(skip)]
    executable_generation_refusal: Option<Box<ExecutableGenerationRefusal>>,
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
            turn_input_acceptance: None,
            foreign_cause: None,
            session_state_version_refusal: None,
            executable_generation_refusal: None,
        }
    }

    /// The refusal of a turn redriven under another executable generation
    /// than its admission recorded (FIG-3571). The turn parks, carrying both
    /// generations.
    #[must_use]
    pub fn retired_generation(refusal: ExecutableGenerationRefusal) -> Self {
        let found = ExecutableGenerationRefusal::spell(refusal.found.as_ref());
        let current = ExecutableGenerationRefusal::spell(refusal.current.as_ref());
        Self::refused_generation(
            refusal,
            format!(
                "the turn was admitted under executable generation {found}, and this build runs \
                 {current}: its redrive was refused before any effect; redrive it under a build \
                 of generation {found}, fork it onto this generation, or cancel it"
            ),
        )
    }

    /// The typed refusal of a process incarnation started under another
    /// executable generation than the one its engine runs now (FIG-3571):
    /// the process parks on it before its first step.
    pub fn retired_process_generation(refusal: ExecutableGenerationRefusal) -> Self {
        let found = ExecutableGenerationRefusal::spell(refusal.found.as_ref());
        let current = ExecutableGenerationRefusal::spell(refusal.current.as_ref());
        Self::refused_generation(
            refusal,
            format!(
                "the process was started under executable generation {found}, and its engine \
                 runs {current} in this build: its redrive was refused before any effect; \
                 redrive it under a build of generation {found}, or cancel it"
            ),
        )
    }

    fn refused_generation(refusal: ExecutableGenerationRefusal, message: String) -> Self {
        let mut error = Self::new(RuntimeErrorCode::RetiredGeneration, message);
        error.executable_generation_refusal = Some(Box::new(refusal));
        error
    }

    /// The generations an executable-generation admission refused, on the
    /// error the refused turn returned. `None` on any other error, and on an
    /// error read back from storage.
    pub fn executable_generation_refusal(&self) -> Option<&ExecutableGenerationRefusal> {
        self.executable_generation_refusal.as_deref()
    }

    #[must_use]
    fn with_session_state_version_refusal(mut self, refusal: SessionStateVersionRefusal) -> Self {
        self.session_state_version_refusal = Some(refusal);
        self
    }

    /// The generations a session-state admission refused, on the error the
    /// refused call returned (FIG-3619). `None` on any other error, and on an
    /// error read back from storage, which keeps only the code and message.
    pub fn session_state_version_refusal(&self) -> Option<SessionStateVersionRefusal> {
        self.session_state_version_refusal
    }

    /// Attaches the acceptance of the direct turn this error aborted.
    #[must_use]
    pub fn with_turn_input_acceptance(
        mut self,
        acceptance: crate::turn_input_vocabulary::TurnInputAcceptanceReceipt,
    ) -> Self {
        self.turn_input_acceptance = Some(Box::new(acceptance));
        self
    }

    /// Constructs an error carrying a code minted outside the built-in
    /// [`RuntimeErrorCode`] vocabulary — a plugin abort or a host effect
    /// completion. The namespaced spelling lands in
    /// [`RuntimeErrorCode::ForeignCode`] verbatim and is never re-parsed into
    /// a built-in arm. First-party producers use [`Self::new`], whose typed
    /// argument makes an unclassified string a compile error.
    pub fn foreign(
        code: impl Into<String>,
        cause: TurnFailureCause,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::ForeignCode(code.into()), message);
        error.foreign_cause = Some(cause);
        error
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

    /// Whether this is a session-retirement refusal: the session was deleted,
    /// or is being deleted, under the turn it failed (FIG-3630).
    ///
    /// Its class is unchanged (terminal, never retried), but a turn failing
    /// with it aborts rather than recording a failed turn: the retirement owns
    /// the turn and leaves no head to record on. Recording would authorize a
    /// cancellation-closure pin the revoked session can never settle, and
    /// that pin refuses the deletion itself.
    pub fn is_session_retirement(&self) -> bool {
        matches!(self.cause, Some(RuntimeErrorCause::SessionDeleted { .. }))
    }

    /// Whether retrying cannot succeed without a host-side change.
    pub fn is_terminal(&self) -> bool {
        self.cause.is_some()
            || match self.foreign_cause {
                Some(cause) => cause == TurnFailureCause::Outcome,
                None => self.code.is_terminal(),
            }
    }

    /// The cause class of a turn this error fails (FIG-3575): an outcome
    /// exactly when the error is terminal.
    pub fn turn_failure_cause(&self) -> TurnFailureCause {
        if self.is_terminal() {
            TurnFailureCause::Outcome
        } else if self.foreign_cause.is_none() && self.code.parks_turn() {
            TurnFailureCause::Parked
        } else {
            TurnFailureCause::LiveFault
        }
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
    /// The kind of the recorded effect whose envelope diverged (its command
    /// `type`, e.g. `llm_call`, `tool_invocation`), so an operator can tell a
    /// model-call drift from a tool or cell drift (FIG-3587).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_kind: Option<String>,
}

/// Journal treatment of an executor failure before a terminal is committed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EffectErrorJournalDisposition {
    #[default]
    Terminal,
    RetryUncommittedResponseDerivation,
}

impl EffectErrorJournalDisposition {
    pub fn is_retryable_derivation(self) -> bool {
        matches!(self, Self::RetryUncommittedResponseDerivation)
    }
}

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    #[serde(skip)]
    journal_disposition: EffectErrorJournalDisposition,
    pub code: RuntimeErrorCode,
    pub message: String,
    /// Boxed, as on [`RuntimeError`]: the rare diagnostic must not size
    /// every effect outcome that carries an error inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Box<crate::RuntimeEffectReplayMismatchReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
    /// `true` when this error is the journaled record of a failed effect —
    /// decoded out of a settled `Failed` terminal — rather than a live fault
    /// that left nothing recorded. Consumers keep a journaled error on the
    /// result surface because it replays identically on every redrive; a
    /// live fault instead aborts like a crash so recovery can re-run the
    /// attempt (FIG-3528). Never persisted: the flag is stamped at the
    /// journal's replay boundary, not read back out of the row.
    #[serde(skip)]
    pub journaled: bool,
    /// The cause class the minting host chose for a foreign code (FIG-3575).
    /// Never persisted, like `journaled`.
    #[serde(skip)]
    foreign_cause: Option<TurnFailureCause>,
}

impl RuntimeEffectControllerError {
    pub fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            journal_disposition: EffectErrorJournalDisposition::Terminal,
            code,
            message: message.into(),
            summary: None,
            cause: None,
            journaled: false,
            foreign_cause: None,
        }
    }

    /// Marks a failed, uncommitted host response derivation as safe to execute again.
    /// This authority is local to the executor; decoding a stored error cannot mint it.
    pub fn retryable_response_derivation(message: impl Into<String>) -> Self {
        let mut error = Self::new(
            RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
            message,
        );
        error.journal_disposition =
            EffectErrorJournalDisposition::RetryUncommittedResponseDerivation;
        error
    }

    /// Marks this failure of an uncommitted host derivation — an
    /// execution-environment sync's rebuild, or a recorded execution-environment
    /// load, that met a live fault — as safe to execute again: the claim is
    /// released unsealed instead of journaling the failure as the effect's
    /// outcome (FIG-3587, FIG-3683).
    #[must_use]
    pub fn retryable_uncommitted_derivation(mut self) -> Self {
        self.journal_disposition =
            EffectErrorJournalDisposition::RetryUncommittedResponseDerivation;
        self
    }

    /// A step whose execution lost its watch on the turn's cancellation gate
    /// (FIG-3672 P9): the watch retried and gave up, so the attempt ends with
    /// this live fault instead of a recorded outcome. It is never journaled
    /// and never read as a cancellation — the engine runs the step again.
    pub fn turn_cancel_watch_lost(message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorCode::TransientCancelWatch, message)
            .retryable_uncommitted_derivation()
    }

    /// Only the host derivations — the assistant-response hooks, the
    /// execution-environment sync and the execution-environment load — and a
    /// drive's admission and seal, a root's scope close and a session's close,
    /// whose store faults are the attempt's (FIG-3600), and a process command
    /// that marked its registry fault retryable (a session deletion's process
    /// cleanup, after its close) can consume derivation retry authority, as
    /// can any step whose cancellation watch was lost
    /// ([`Self::turn_cancel_watch_lost`]): that fault is about the attempt,
    /// never the step.
    pub fn journal_disposition(&self, kind: RuntimeEffectKind) -> EffectErrorJournalDisposition {
        if matches!(
            kind,
            RuntimeEffectKind::AssistantResponseHooks
                | RuntimeEffectKind::SyncExecutionEnvironment
                | RuntimeEffectKind::LoadExecutionEnv
                | RuntimeEffectKind::AdmitDrive
                | RuntimeEffectKind::SealDriveAdmission
                | RuntimeEffectKind::ClaimAcceptedTurnInput
                | RuntimeEffectKind::CloseRootScope
                | RuntimeEffectKind::BeginSessionClose
                | RuntimeEffectKind::Process
        ) || self.code == RuntimeErrorCode::TransientCancelWatch
        {
            self.journal_disposition
        } else {
            EffectErrorJournalDisposition::Terminal
        }
    }

    /// Hosts must namespace these codes and must not mint a built-in
    /// [`RuntimeErrorCode`] spelling. First-party producers use [`Self::new`],
    /// whose typed argument makes an unclassified string a compile error.
    pub fn foreign(
        code: impl Into<String>,
        cause: TurnFailureCause,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::ForeignCode(code.into()), message);
        error.foreign_cause = Some(cause);
        error
    }

    /// Marks this error as the journaled record of a failed effect — the
    /// `Failed` terminal a settled row replays — so consumers surface it as
    /// the attempt's recorded outcome instead of treating it as a live
    /// journal fault.
    pub fn into_journaled(mut self) -> Self {
        self.journaled = true;
        self
    }

    /// Whether this is a session-retirement refusal: the session was deleted,
    /// or is being deleted, under the turn it failed (FIG-3630).
    ///
    /// Its class is unchanged (terminal, never retried), but a turn failing
    /// with it aborts rather than recording a failed turn: the retirement owns
    /// the turn and leaves no head to record on. Recording would authorize a
    /// cancellation-closure pin the revoked session can never settle, and
    /// that pin refuses the deletion itself.
    pub fn is_session_retirement(&self) -> bool {
        matches!(self.cause, Some(RuntimeErrorCause::SessionDeleted { .. }))
    }

    /// Whether retrying cannot succeed without a host-side change: a terminal
    /// cause, a foreign code its host minted as an outcome, or a terminal
    /// code.
    pub fn is_terminal(&self) -> bool {
        self.cause.is_some()
            || match self.foreign_cause {
                Some(cause) => cause == TurnFailureCause::Outcome,
                None => self.code.is_terminal(),
            }
    }

    /// The cause class of a turn this error fails (FIG-3575).
    ///
    /// A journaled error is the recorded outcome of its effect whatever its
    /// code (FIG-3528: journaled outcomes stay on the result surface), so a
    /// redrive replays it as the same recorded failure instead of aborting on
    /// it forever. Any other error is an outcome exactly when it is terminal.
    pub fn turn_failure_cause(&self) -> TurnFailureCause {
        if self.journaled || self.cause.is_some() {
            return TurnFailureCause::Outcome;
        }
        match self.foreign_cause {
            Some(cause) => cause,
            None => self.code.turn_failure_cause(),
        }
    }

    /// Sets the summary carried by a `RuntimeEffectControllerError` for effect-host implementors
    /// while executing or replaying a runtime effect.
    pub fn with_summary(mut self, summary: crate::RuntimeEffectReplayMismatchReport) -> Self {
        self.summary = Some(Box::new(summary));
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
            journal_disposition: _,
            code,
            message,
            summary,
            cause,
            journaled: _,
            foreign_cause,
        } = self;
        let mut runtime = RuntimeError::new(code, message);
        runtime.summary = summary;
        runtime.foreign_cause = foreign_cause;
        match cause {
            Some(cause) => runtime.with_cause(cause),
            None => runtime,
        }
    }
}

impl From<RuntimeError> for RuntimeEffectControllerError {
    fn from(err: RuntimeError) -> Self {
        Self {
            journal_disposition: EffectErrorJournalDisposition::Terminal,
            code: err.code,
            message: err.message,
            summary: err.summary,
            cause: err.cause,
            journaled: false,
            foreign_cause: err.foreign_cause,
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
            crate::StoreError::SessionStateVersionUnsupported { .. } => {
                crate::RuntimeErrorCode::SessionStateVersionUnsupported
            }
            crate::StoreError::SessionStateVersionNewerThanRuntime { .. } => {
                crate::RuntimeErrorCode::SessionStateVersionNewerThanRuntime
            }
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
            journal_disposition: EffectErrorJournalDisposition::Terminal,
            code,
            message: err.to_string(),
            summary: None,
            cause,
            journaled: false,
            foreign_cause: None,
        }
    }
}
