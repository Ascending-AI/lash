//! Kernel error vocabulary.
//!
//! `RuntimeError` and its code enum are the typed failure surface every
//! durable operation reports, and they name `StoreError` directly, so they
//! live beside it. The session-facing mapping into `SessionError` stays in
//! `lash-core`.

pub use crate::executable_generation::{ExecutableGeneration, ExecutableGenerationRefusal};
use crate::{RuntimeEffectKind, SessionId};
use serde::{Deserialize, Serialize};

mod attachment_retention;
pub use attachment_retention::{AttachmentRetentionFailure, AttachmentRetentionStoreFailure};
mod cause;
mod classification;
pub use cause::{GroupChildCapability, RuntimeErrorCause, StoredDataCorruption};
pub(crate) mod model_unavailable;
mod run_shape;
mod tool_call_limit;
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
    /// An artifact publish or acquire named a referrer that has a fence
    /// (ADR 0113 §2.7). Store implementors return this code instead of
    /// wording the refusal as prose; the error carries the referrer.
    ArtifactReferrerEnded,
    /// An artifact acquire named bytes that are not stored.
    ArtifactMissing,
    ReferrerKindRefused,
    /// A process definition id names no stored descriptor: nothing holds it
    /// (ADR 0113 §3.6). A missing definition is never an empty success.
    DefinitionMissing,
    /// The owning engine refuses a stored or published definition: no engine
    /// of its kind, a value it cannot resolve, a forged signature claim, or a
    /// manifest that disagrees with the engine's resolution.
    DefinitionRefused,
    /// This host could not obtain a worker before the checkout deadline.
    /// No guest or definition verdict was produced; the attempt can retry.
    WorkerCheckoutTimedOut,
    /// A worker fault whose typed cause distinguishes a retry from a run refusal.
    VmWorkerFailed,
    /// This deployment cannot launch its configured worker. Repair and redrive.
    VmWorkerUnavailable,
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
    /// A session's drive admission found a turn input while the parked
    /// root's park still names a redrive intent that is not yet settled
    /// (D15). The input is refused rather than interleaved with the
    /// redrive; the refusal is never a recorded verdict, so the identical
    /// admission is safe to retry and admits the input once the redrive
    /// settles.
    SessionRedriveUnsettled,
    /// A head write outside every drive found the session head owned
    /// (FIG-4202): a bound root, an owed follow-on or an open session
    /// command. Nothing was written. The owner releases the head at its
    /// boundary, so the identical write is safe to retry then; a host moves
    /// the head through a session command instead.
    SessionHeadOwned,
    /// A host write that moves a store-backed session's head was called
    /// directly (FIG-4202): the bound turn owns the head, so the write is a
    /// session command the drive applies at a turn boundary. Submit it with
    /// `submit_session_command` and await its settlement; only a storeless
    /// runtime writes directly.
    SessionCommandRequired,
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
    /// Session work waits for an unfinished root or its owed follow-on.
    SessionRootPending,
    /// A pending follow-on owns the session (ADR 0101 §3): the commit or
    /// frame change is refused until the follow-on's own terminal commit.
    FollowOnPending,
    /// The final runtime commit lost the session-head compare-and-swap to a
    /// newer commit. Nothing from the losing commit was published, but the
    /// identical stale commit is not safe to retry: reload the durable head and
    /// re-establish current drive and root authority before building new work
    /// (ADR 0101).
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
    /// A session's config was refused at its creation: a namespace no
    /// installed owner registers, or a value its owner refuses (FIG-4379).
    /// The deployment's plugin set and the recorded request decide it, so
    /// retrying the identical creation cannot change the answer (FIG-4396).
    SessionConfigRefused,
    /// A session's catalog row has no head, so its creation recorded no
    /// config (FIG-4553). A session opens with the config its creation
    /// recorded and never with defaults, so the open is refused; a redrive
    /// reads the same row.
    SessionCreationUnrecorded,
    /// A root process start holds a host session-lookup grant for a session
    /// the catalog does not hold live when the start's recorded admission
    /// runs. The admission records the refusal, so a replay answers it too.
    HostSessionNotLive,
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
    /// Terminal assembly has no installed root record from which to read
    /// its termination policy. Repair the root's recorded view before it
    /// can commit; the worker's live policy cannot replace that record.
    RecordedTerminationUnavailable,
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
    /// An administrative compaction failed before it opened its frame: its
    /// compactor, its prompt or its frame open refused (FIG-4201). The
    /// command that carried it settles with this code and is never applied
    /// again.
    ContextCompaction,
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
    /// A send, a create request or a config command named a model key the
    /// host's registry does not register: a send and a create request are
    /// refused typed before anything is accepted, a config command is
    /// refused when its transaction resolves. Nothing changes either way.
    ModelUnknown,
    /// A send, a root's run spec or a child's create request selects
    /// reasoning the capability of the model it would run refuses
    /// (FIG-4531). It is refused where it is stated, before anything runs:
    /// the same selection over the same recorded capability is refused
    /// again.
    ReasoningRefused,
    /// A model a root recorded, or an admitted root's per-run key, has no
    /// binding on this worker. The model was adopted when it was set, so this
    /// is the worker's deployment, not the session's intent: the engine
    /// retries the root, and its retry budget parks it.
    ModelUnavailable,
    /// A recorded config selects no model, so the work it governs has
    /// nothing to run a model call with. It is a recorded absence no
    /// deployment can repair, so it is the work's outcome and is never
    /// retried, unlike [`Self::ModelUnavailable`].
    ModelUnconfigured,
    /// A root's run spec names a definition revision this worker does not
    /// register (FIG-3838). It is the deployment, not the input: the root
    /// retries, its retry budget parks it, and a redeploy that registers the
    /// revision recovers it. Nothing is recorded, and no other revision is
    /// ever used instead.
    RunDefinitionUnavailable,
    /// A recorded renderer is absent on this worker. Redeploying it can resume the root.
    RecordedRendererUnavailable,
    /// An output too long for history could not be retained as a session
    /// attachment before it entered history (FIG-1643). The output never
    /// enters history in its place: the step retries, and its retry budget
    /// parks the root.
    OutputRetentionFailed,
    /// A required output retention was permanently refused by its attachment store.
    OutputRetentionRefused,
    /// A registered run definition refused the spec's context (FIG-3838):
    /// deterministic, so it is recorded as the root's failure.
    RunShapeRefused,
    /// An input addressed to a running turn carried an explicit run spec
    /// that differs from the turn's (FIG-3838): refused before acceptance.
    RunSpecMismatch,
    /// An input addressed a turn that is neither its session's running turn
    /// nor one with its final commit recorded (ADR 0101 §5.1): refused before
    /// acceptance, with no row and no sequence number.
    TurnAddressUnknown,
    /// Admission refused a source key reserved for another ingress kind.
    IngressReservedSourceKey,
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
    /// A session-only operation was asked of a process runtime: a process
    /// has no session and no agent frame of its own.
    NotASessionRuntime,
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
    /// A host start key is bound to a retained process that another start
    /// made (ADR 0107): the retry presented a different start. The refusal
    /// names the key and nothing else, since the key is global and the
    /// retained process may be another originator's.
    ProcessStartKeyConflict,
    /// A process cannot run without the configuration recorded at creation.
    MissingRecordedProcessConfig,
    /// A host rail was handed a start key of a family lash derives for its
    /// own start paths (ADR 0107): a host mints only host keys.
    StartKeyFamilyRefused,
    /// A trigger delivery's start found no retained process under its key,
    /// and the delivery already bound to one (ADR 0107 §5, FIG-4369): the
    /// bound process was pruned, and the start registers nothing.
    TriggerDeliveryBound,
    /// A trigger delivery's start found no retained process under its key,
    /// and no delivery row (FIG-4369): retention removed the delivery once
    /// its bound process was pruned, and the start registers nothing.
    TriggerDeliveryRetired,
    /// An ingest named an occurrence retention reclaimed (FIG-4513).
    TriggerOccurrenceReclaimed,
    /// A trigger delivery's start asked the host to restore its captured
    /// provider route, and the provider did not answer (FIG-4554). Nothing
    /// registered; the reservation stays owed, and its recovery asks again
    /// under the same identity.
    TriggerRouteUnavailable,
    /// A trigger delivery's start asked the host to restore its captured
    /// provider route, and the provider revoked or refuses it (FIG-4554).
    /// Nothing registered, and nothing re-resolves the source.
    TriggerRouteRevoked,
    /// ADR 0051 effect-host implementor diagnostic for a process-command
    /// refusal whose terminal target has been replaced by a retention tombstone.
    ProcessNoLongerRetained,
    ProcessRegistryUnavailable,
    ProcessSignalWaitCancelled,
    /// A process segment's signal wait was handed to a successor segment on
    /// the drain's wake (FIG-3799): the wait stays open and the body stops on
    /// it, for its continuation to wait again. Never a guest-visible outcome.
    ProcessSignalWaitHandedOver,
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
    /// The engine ended a root's only run without a Lash outcome.
    EngineRootSubstrateLost,
    /// A control verb or drive request did not reach the engine, or the engine
    /// did not carry it out. The refusal's disposition says whether asking
    /// again can succeed.
    EngineControlRequest,
    /// The installed engine does not implement the control verb asked of it.
    EngineControlUnsupported,
    /// A park's stored engine handle does not name an execution of the root's
    /// session, so the engine resumes or releases nothing under it.
    EngineHandleMismatch,
    /// A process redrive named a process that holds no park.
    ProcessNotParked,
    /// A process redrive named a park the process has since replaced.
    ProcessParkSuperseded,
    /// A relay was handed an obligation whose key belongs to another ledger.
    ObligationKeyMismatch,
    /// The row an obligation lives on lacks the durable evidence its delivery
    /// needs, so the delivery can never be made as armed.
    ObligationRowInvariant,
    /// One obligation delivery attempt ran past its kind's attempt budget and
    /// was abandoned.
    ObligationAttemptBudgetExceeded,
    /// The engine accepted every ask of a consumer-settled obligation and no
    /// consumer settled it.
    ObligationAskUnadmitted,
    /// A delivery's claim was retaken by another relay before the delivery
    /// settled what it owed.
    ObligationClaimLost,
    /// A closing session's physical delete waits on cleanup that has not
    /// settled.
    SessionDeleteCleanupPending,
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
    /// A child's final was committed at the group's durable linearization
    /// point by an invocation that ended before its seat, and the invocation
    /// seating it cannot realize that final: the committed outcome and any
    /// intents it declared are reported lost rather than replaced by the
    /// seating invocation's own refusal (ADR 0099 §5).
    RuntimeEffectGroupChildCommittedFinalLost,
    /// The deployment serving the child's lane lacks a capability the child
    /// needs, named by [`RuntimeErrorCause::EffectGroupChildUnroutable`].
    RuntimeEffectGroupChildUnroutable,
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
    /// A tool call would take its cell or process past the session's
    /// recorded `max_tool_calls` (ADR 0099 §9, FIG-4546). Refused whole,
    /// before any child is dispatched; the typed half is
    /// [`RuntimeErrorCause::MaxToolCallsExceeded`]. The program's failure,
    /// not the host's: every replay refuses the same call.
    MaxToolCallsExceeded,
    RuntimeEffectInvocationSubject,
    RuntimeEffectScopeMismatch,
    RuntimeEffectLocalExecutorMismatch,
    RuntimeEffectLocalExecutorUnavailable,
    RuntimeEffectLocalTaskClosed,
    RuntimeEffectProcessTaskJoin,
    RuntimeEffectReplayRequired,
    RuntimeEffectSleepCancelled,
    RuntimeEffectTaskJoin,
    RuntimeEffectToolAttemptCaptureVersion,
    RuntimeEffectToolAttemptIndex,
    RuntimeEffectToolChildCancellationAuthority,
    RuntimeEffectToolChildCompletionRouting,
    RuntimeEffectToolChildRequestAdmission,
    RuntimeEffectToolChildRequestOpener,
    RuntimeEffectToolChildRequestVersion,
    RuntimeEffectToolSettlementVersion,
    /// A provider call was dispatched outside any spending effect's usage run
    /// (ADR 0125): nothing would account for it, so it is refused before
    /// dispatch.
    UsageRunMissing,
    /// Admitting a spending effect's usage run to storage failed. The attempt
    /// ends retryably and journals nothing; the engine runs it again.
    UsageAdmissionFault,
    /// The accounting owner is retired; no further provider dispatch may spend under it.
    UsageOwnerRetired,
    RuntimeEffectWrongOutcome,
    /// Process-local; repaired by restart, not by same-process retry.
    RuntimeEffectControllerTaskClosed,
    /// A newer fleet epoch excludes this deployment's writable range.
    WriterFenced,
    /// The store's compatibility stamp or fleet format cannot be admitted.
    StoreIncompatible,
    /// A store returned state belonging to another session.
    StoreSessionMismatch,
    /// The deployment runs over stores whose session admitted another
    /// cancellation authority.
    TurnCancelBindingMismatch,
    /// A catalog retained a binding to an effect host that has ended.
    /// Rebind the catalog before authorizing or retiring cancellation work.
    TurnCancelClosureOwnerReleased,
    RuntimeStore,
    /// Durable state is corrupt or an authoritative monotonic counter has
    /// exhausted its representable domain. Retrying unchanged cannot heal it.
    RuntimeStoreCorrupt,
    SessionCommandRun,
    SessionCommandIdempotencyKey,
    SessionCommandPostDriveRefresh,
    SessionCommandRefresh,
    SessionCommandRefreshTools,
    SessionDeleteScopeMismatch,
    SessionHeadRefresh,
    SessionToolRegistry,
    /// Process-local; repaired by restart, not by same-process retry.
    ToolCatalogResolutionFailed,
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
/// [`RuntimeErrorCode::DurableIdentityConflict`]; a closing or deleted
/// session's refusal surfaces as [`RuntimeErrorCode::SessionDeleted`] with its
/// cause (ADR 0109 §4); a superseded drive fence surfaces as
/// [`RuntimeErrorCode::StoreCommitSuperseded`]; every other admission failure
/// is a store commit failure.
pub fn runtime_error_from_turn_input_admission(err: crate::store::StoreError) -> RuntimeError {
    match err {
        err @ crate::store::StoreError::StoredDataCorrupt { .. } => {
            RuntimeEffectControllerError::from(err).into_runtime_error()
        }
        err if crate::store::StoreRefusal::of_store_error(&err).is_some() => {
            RuntimeEffectControllerError::from(err).into_runtime_error()
        }
        err @ (crate::store::StoreError::PendingTurnInputSourceKeyConflict { .. }
        | crate::store::StoreError::QueuedWorkSourceKeyConflict { .. }
        | crate::store::StoreError::PendingTurnInputIdConflict { .. }
        | crate::store::StoreError::PendingTurnInputBatchDuplicate { .. }
        | crate::store::StoreError::RunSpecHashCollision { .. }) => {
            RuntimeError::new(RuntimeErrorCode::DurableIdentityConflict, err.to_string())
        }
        err @ crate::store::StoreError::PendingTurnInputRunSpecMismatch { .. } => {
            RuntimeError::new(RuntimeErrorCode::RunSpecMismatch, err.to_string())
        }
        err @ crate::store::StoreError::IngressTurnAddressUnknown { .. } => {
            RuntimeError::new(RuntimeErrorCode::TurnAddressUnknown, err.to_string())
        }
        ref err @ crate::store::StoreError::IngressReservedSourceKey {
            ref session_id,
            kind,
            ref source_key,
        } => RuntimeError::new(RuntimeErrorCode::IngressReservedSourceKey, err.to_string())
            .with_cause(RuntimeErrorCause::IngressReservedSourceKey {
                refusal: Box::new(IngressReservedSourceKeyRefusal {
                    session_id: session_id.clone(),
                    ingress_kind: kind.to_owned(),
                    source_key: source_key.clone(),
                }),
            }),
        err @ (crate::store::StoreError::SessionClosing { .. }
        | crate::store::StoreError::SessionDeleted { .. }
        | crate::store::StoreError::StaleDriveFence { .. }) => runtime_error_from_store_commit(err),
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
}

pub fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    match err {
        err @ crate::store::StoreError::StoredDataCorrupt { .. } => {
            RuntimeEffectControllerError::from(err).into_runtime_error()
        }
        err if crate::store::StoreRefusal::of_store_error(&err).is_some() => {
            RuntimeEffectControllerError::from(err).into_runtime_error()
        }
        err @ (crate::store::StoreError::PendingTurnInputSourceKeyConflict { .. }
        | crate::store::StoreError::QueuedWorkSourceKeyConflict { .. }
        | crate::store::StoreError::PendingTurnInputIdConflict { .. }
        | crate::store::StoreError::PendingTurnInputBatchDuplicate { .. }
        | crate::store::StoreError::RunSpecHashCollision { .. }
        | crate::store::StoreError::PendingTurnInputRunSpecMismatch { .. }
        | crate::store::StoreError::IngressTurnAddressUnknown { .. }
        | crate::store::StoreError::IngressReservedSourceKey { .. }) => {
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
        // A stale drive fence never commits again: a later drive owns the session (ADR 0105 §9).
        err @ (crate::store::StoreError::TurnCancelIntentChanged { .. }
        | crate::store::StoreError::StaleDriveFence { .. }) => {
            RuntimeError::new(RuntimeErrorCode::StoreCommitSuperseded, err.to_string())
        }
        ref err @ crate::store::StoreError::SessionDeleted { ref session_id } => {
            RuntimeError::new(RuntimeErrorCode::SessionDeleted, err.to_string()).with_cause(
                RuntimeErrorCause::SessionDeleted {
                    session_id: session_id.clone(),
                },
            )
        }
        // A closing session is past its point of no return: to a caller it is already gone.
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
        crate::store::StoreError::SessionExecutionLeaseExpired { session_id } => RuntimeError::new(
            RuntimeErrorCode::SessionExecutionLeaseLost,
            format!("session execution lease for session `{session_id}` was lost before commit"),
        ),
        crate::store::StoreError::ExecutionStateCaptureFailed { message } => RuntimeError::new(
            RuntimeErrorCode::ExecutionStateCaptureFailed,
            format!("failed to snapshot dirty execution state: {message}"),
        ),
        crate::store::StoreError::TurnOutcomeMaterializationRefused { error } => *error,
        ref err @ (crate::store::StoreError::FollowOnPending { .. }
        | crate::store::StoreError::FollowOnFrameNotCurrent { .. }
        | crate::store::StoreError::FollowOnNotPending { .. }) => {
            RuntimeError::new(RuntimeErrorCode::FollowOnPending, err.to_string())
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
            Self::ArtifactReferrerEnded => "artifact_referrer_ended",
            Self::ArtifactMissing => "artifact_missing",
            Self::ReferrerKindRefused => "referrer_kind_refused",
            Self::DefinitionMissing => "definition_missing",
            Self::DefinitionRefused => "definition_refused",
            Self::WorkerCheckoutTimedOut => "worker_checkout_timed_out",
            Self::VmWorkerFailed => "vm_worker_failed",
            Self::VmWorkerUnavailable => "vm_worker_unavailable",
            Self::EffectPanicked => "effect_panicked",
            Self::MissingExecutionScopeId => "missing_execution_scope_id",
            Self::ExecutionScopeTurnIdMismatch => "execution_scope_turn_id_mismatch",
            Self::ExecutionScopeAdmissionRefused => "execution_scope_admission_refused",
            Self::SessionExecutionLeaseLost => "session_execution_lease_lost",
            Self::SessionExecutionLaneBusy => "session_execution_lane_busy",
            Self::SessionRedriveUnsettled => "session_redrive_unsettled",
            Self::SessionHeadOwned => "session_head_owned",
            Self::SessionCommandRequired => "session_command_required",
            Self::AcceptedTurnInputCeded => "accepted_turn_input_ceded",
            Self::SessionWorkUnavailable => "session_work_unavailable",
            Self::TurnExecutionRequiresReconciledToolSurface => {
                "turn_execution_requires_reconciled_tool_surface"
            }
            Self::StoreCommitContended => "store_commit_contended",
            Self::SessionRootPending => "session_root_pending",
            Self::FollowOnPending => "follow_on_pending",
            Self::StoreCommitSuperseded => "store_commit_superseded",
            Self::SessionDeleted => "session_deleted",
            Self::SessionCatalogLookupUnsupported => "session_catalog_lookup_unsupported",
            Self::SessionConfigRefused => "session_config_refused",
            Self::SessionCreationUnrecorded => "session_creation_unrecorded",
            Self::HostSessionNotLive => "host_session_not_live",
            Self::SessionStateVersionUnsupported => "session_state_version_unsupported",
            Self::SessionStateVersionNewerThanRuntime => "session_state_version_newer_than_runtime",
            Self::StoreCommitNodeBudgetExceeded => "store_commit_node_budget_exceeded",
            Self::StoreCommitByteBudgetExceeded => "store_commit_byte_budget_exceeded",
            Self::CheckpointComponentEncodingVersionMismatch => {
                "checkpoint_component_encoding_version_mismatch"
            }
            Self::RecordEncodingFailed => "record_encoding_failed",
            Self::RecordedTerminationUnavailable => "recorded_termination_unavailable",
            Self::MissingProcessExecutionId => "missing_process_execution_id",
            Self::ExecutionStateCaptureFailed => "execution_state_capture_failed",
            Self::ResidentSessionReloadFailed => "resident_session_reload_failed",
            Self::StoreCommitFailed => "store_commit_failed",
            Self::PluginSessionManager => "plugin_session_manager",
            Self::PluginFinalizeTurn => "plugin_finalize_turn",
            Self::PluginCheckpoint => "plugin_checkpoint",
            Self::PluginPrepareTurn => "plugin_prepare_turn",
            Self::ContextPrepareTurn => "context_prepare_turn",
            Self::ContextCompaction => "context_compaction",
            Self::ProtocolBeforeLlmCall => "protocol_before_llm_call",
            Self::TurnStreamJoin => "turn_stream_join",
            Self::EmptyAgentFrameRun => "empty_agent_frame_run",
            Self::HistoricalAgentFrameSwitchUnsupported => {
                "historical_agent_frame_switch_unsupported"
            }
            Self::AgentFrameSwitchAuthorConflict => "agent_frame_switch_author_conflict",
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
            Self::ModelUnknown => "model_unknown",
            Self::ReasoningRefused => "reasoning_refused",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelUnconfigured => "model_unconfigured",
            Self::RunDefinitionUnavailable => "run_definition_unavailable",
            Self::RecordedRendererUnavailable => "recorded_renderer_unavailable",
            Self::OutputRetentionFailed => "output_retention_failed",
            Self::OutputRetentionRefused => "output_retention_refused",
            Self::RunShapeRefused => "run_shape_refused",
            Self::RunSpecMismatch => "run_spec_mismatch",
            Self::TurnAddressUnknown => "turn_address_unknown",
            Self::IngressReservedSourceKey => "ingress_reserved_source_key",
            Self::Plugin => "plugin",
            Self::QueuedWork => "queued_work",
            Self::QueuedWorkRowExceedsContextWindow => "queued_work_row_exceeds_context_window",
            Self::ProcessPanicked => "process_panicked",
            Self::ProcessNotVisible => "process_not_visible",
            Self::NotASessionRuntime => "not_a_session_runtime",
            Self::ProcessAlreadyTerminal => "process_already_terminal",
            Self::ProcessParentEnded => "process_parent_ended",
            Self::ProcessCancelConflict => "process_cancel_conflict",
            Self::DurableIdentityConflict => "durable_identity_conflict",
            Self::ProcessStartKeyConflict => "process_start_key_conflict",
            Self::MissingRecordedProcessConfig => "missing_recorded_process_config",
            Self::StartKeyFamilyRefused => "start_key_family_refused",
            Self::TriggerDeliveryBound => "trigger_delivery_bound",
            Self::TriggerDeliveryRetired => "trigger_delivery_retired",
            Self::TriggerOccurrenceReclaimed => "trigger_occurrence_reclaimed",
            Self::TriggerRouteUnavailable => "trigger_route_unavailable",
            Self::TriggerRouteRevoked => "trigger_route_revoked",
            Self::ProcessNoLongerRetained => "process_no_longer_retained",
            Self::ProcessRegistryUnavailable => "process_registry_unavailable",
            Self::ProcessSignalWaitCancelled => "process_signal_wait_cancelled",
            Self::ProcessSignalWaitHandedOver => "process_signal_wait_handed_over",
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
            Self::EngineRootSubstrateLost => "engine_root_substrate_lost",
            Self::EngineControlRequest => "engine_control_request",
            Self::EngineControlUnsupported => "engine_control_unsupported",
            Self::EngineHandleMismatch => "engine_handle_mismatch",
            Self::ProcessNotParked => "process_not_parked",
            Self::ProcessParkSuperseded => "process_park_superseded",
            Self::ObligationKeyMismatch => "obligation_key_mismatch",
            Self::ObligationRowInvariant => "obligation_row_invariant",
            Self::ObligationAttemptBudgetExceeded => "obligation_attempt_budget_exceeded",
            Self::ObligationAskUnadmitted => "obligation_ask_unadmitted",
            Self::ObligationClaimLost => "obligation_claim_lost",
            Self::SessionDeleteCleanupPending => "session_delete_cleanup_pending",
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
            Self::RuntimeEffectGroupChildCommittedFinalLost => {
                "runtime_effect_group_child_committed_final_lost"
            }
            Self::RuntimeEffectGroupChildUnroutable => "runtime_effect_group_child_unroutable",
            Self::RuntimeEffectGroupDrainDeferred => "runtime_effect_group_drain_deferred",
            Self::RuntimeEffectGroupShape => "runtime_effect_group_shape",
            Self::AggregateAwaitUnsettled => "aggregate_await_unsettled",
            Self::MaxToolCallsExceeded => "max_tool_calls_exceeded",
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
            Self::RuntimeEffectToolChildRequestOpener => "runtime_effect_tool_child_request_opener",
            Self::UsageRunMissing => "usage_run_missing",
            Self::UsageAdmissionFault => "usage_admission_fault",
            Self::UsageOwnerRetired => "usage_owner_retired",
            Self::RuntimeEffectToolChildRequestVersion => {
                "runtime_effect_tool_child_request_version"
            }
            Self::RuntimeEffectToolSettlementVersion => "runtime_effect_tool_settlement_version",
            Self::RuntimeEffectWrongOutcome => "runtime_effect_wrong_outcome",
            Self::RuntimeEffectControllerTaskClosed => "runtime_effect_controller_task_closed",
            Self::WriterFenced => "writer_fenced",
            Self::TurnCancelBindingMismatch => "turn_cancel_binding_mismatch",
            Self::TurnCancelClosureOwnerReleased => "turn_cancel_closure_owner_released",
            Self::StoreIncompatible => "store_incompatible",
            Self::StoreSessionMismatch => "store_session_mismatch",
            Self::RuntimeStore => "runtime_store",
            Self::RuntimeStoreCorrupt => "runtime_store_corrupt",
            Self::SessionCommandRun => "session_command_run",
            Self::SessionCommandIdempotencyKey => "session_command_idempotency_key",
            Self::SessionCommandPostDriveRefresh => "session_command_post_drive_refresh",
            Self::SessionCommandRefresh => "session_command_refresh",
            Self::SessionCommandRefreshTools => "session_command_refresh_tools",
            Self::SessionDeleteScopeMismatch => "session_delete_scope_mismatch",
            Self::SessionHeadRefresh => "session_head_refresh",
            Self::SessionToolRegistry => "session_tool_registry",
            Self::ToolCatalogResolutionFailed => "tool_catalog_resolution_failed",
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
    /// Hosts use this predicate for alerting and drain policy.
    pub fn is_replay_mismatch(&self) -> bool {
        matches!(
            self,
            |Self::EngineProcessJournalIdentityDrift| Self::EffectReplayDivergence
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

    /// Built-in strings are always canonicalized to their dedicated variants;
    /// only unknown extension strings produce [`Self::ForeignCode`], as a
    /// recorded outcome. A host minting a live fault under its own code builds
    /// the error with [`RuntimeError::foreign`], which takes its cause class.
    pub fn from_wire_code(code: &str) -> Self {
        match code {
            "attachment_source_policy_denied" => Self::AttachmentSourcePolicyDenied,
            "artifact_referrer_ended" => Self::ArtifactReferrerEnded,
            "artifact_missing" => Self::ArtifactMissing,
            "referrer_kind_refused" => Self::ReferrerKindRefused,
            "definition_missing" => Self::DefinitionMissing,
            "definition_refused" => Self::DefinitionRefused,
            "worker_checkout_timed_out" => Self::WorkerCheckoutTimedOut,
            "vm_worker_failed" => Self::VmWorkerFailed,
            "vm_worker_unavailable" => Self::VmWorkerUnavailable,
            "effect_panicked" => Self::EffectPanicked,
            "missing_execution_scope_id" => Self::MissingExecutionScopeId,
            "execution_scope_turn_id_mismatch" => Self::ExecutionScopeTurnIdMismatch,
            "execution_scope_admission_refused" => Self::ExecutionScopeAdmissionRefused,
            "session_execution_lease_lost" => Self::SessionExecutionLeaseLost,
            "session_execution_lane_busy" => Self::SessionExecutionLaneBusy,
            "session_redrive_unsettled" => Self::SessionRedriveUnsettled,
            "session_head_owned" => Self::SessionHeadOwned,
            "session_command_required" => Self::SessionCommandRequired,
            "accepted_turn_input_ceded" => Self::AcceptedTurnInputCeded,
            "session_work_unavailable" => Self::SessionWorkUnavailable,
            "turn_execution_requires_reconciled_tool_surface" => {
                Self::TurnExecutionRequiresReconciledToolSurface
            }
            "store_commit_contended" => Self::StoreCommitContended,
            "session_root_pending" => Self::SessionRootPending,
            "follow_on_pending" => Self::FollowOnPending,
            "store_commit_superseded" => Self::StoreCommitSuperseded,
            "session_deleted" => Self::SessionDeleted,
            "session_catalog_lookup_unsupported" => Self::SessionCatalogLookupUnsupported,
            "session_config_refused" => Self::SessionConfigRefused,
            "session_creation_unrecorded" => Self::SessionCreationUnrecorded,
            "host_session_not_live" => Self::HostSessionNotLive,
            "session_state_version_unsupported" => Self::SessionStateVersionUnsupported,
            "session_state_version_newer_than_runtime" => Self::SessionStateVersionNewerThanRuntime,
            "store_commit_node_budget_exceeded" => Self::StoreCommitNodeBudgetExceeded,
            "store_commit_byte_budget_exceeded" => Self::StoreCommitByteBudgetExceeded,
            "checkpoint_component_encoding_version_mismatch" => {
                Self::CheckpointComponentEncodingVersionMismatch
            }
            "record_encoding_failed" => Self::RecordEncodingFailed,
            "recorded_termination_unavailable" => Self::RecordedTerminationUnavailable,
            "missing_process_execution_id" => Self::MissingProcessExecutionId,
            "execution_state_capture_failed" => Self::ExecutionStateCaptureFailed,
            "resident_session_reload_failed" => Self::ResidentSessionReloadFailed,
            "store_commit_failed" => Self::StoreCommitFailed,
            "plugin_session_manager" => Self::PluginSessionManager,
            "plugin_finalize_turn" => Self::PluginFinalizeTurn,
            "plugin_checkpoint" => Self::PluginCheckpoint,
            "plugin_prepare_turn" => Self::PluginPrepareTurn,
            "context_prepare_turn" => Self::ContextPrepareTurn,
            "context_compaction" => Self::ContextCompaction,
            "protocol_before_llm_call" => Self::ProtocolBeforeLlmCall,
            "turn_stream_join" => Self::TurnStreamJoin,
            "empty_agent_frame_run" => Self::EmptyAgentFrameRun,
            "historical_agent_frame_switch_unsupported" => {
                Self::HistoricalAgentFrameSwitchUnsupported
            }
            "agent_frame_switch_author_conflict" => Self::AgentFrameSwitchAuthorConflict,
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
            "model_unknown" => Self::ModelUnknown,
            "reasoning_refused" => Self::ReasoningRefused,
            "model_unavailable" => Self::ModelUnavailable,
            "model_unconfigured" => Self::ModelUnconfigured,
            "run_definition_unavailable" => Self::RunDefinitionUnavailable,
            "recorded_renderer_unavailable" => Self::RecordedRendererUnavailable,
            "output_retention_failed" => Self::OutputRetentionFailed,
            "output_retention_refused" => Self::OutputRetentionRefused,
            "run_shape_refused" => Self::RunShapeRefused,
            "run_spec_mismatch" => Self::RunSpecMismatch,
            "turn_address_unknown" => Self::TurnAddressUnknown,
            "ingress_reserved_source_key" => Self::IngressReservedSourceKey,
            "plugin" => Self::Plugin,
            "queued_work" => Self::QueuedWork,
            "queued_work_row_exceeds_context_window" => Self::QueuedWorkRowExceedsContextWindow,
            "process_panicked" => Self::ProcessPanicked,
            "process_not_visible" => Self::ProcessNotVisible,
            "not_a_session_runtime" => Self::NotASessionRuntime,
            "process_already_terminal" => Self::ProcessAlreadyTerminal,
            "process_parent_ended" => Self::ProcessParentEnded,
            "process_cancel_conflict" => Self::ProcessCancelConflict,
            "durable_identity_conflict" => Self::DurableIdentityConflict,
            "process_start_key_conflict" => Self::ProcessStartKeyConflict,
            "missing_recorded_process_config" => Self::MissingRecordedProcessConfig,
            "start_key_family_refused" => Self::StartKeyFamilyRefused,
            "trigger_delivery_bound" => Self::TriggerDeliveryBound,
            "trigger_delivery_retired" => Self::TriggerDeliveryRetired,
            "trigger_occurrence_reclaimed" => Self::TriggerOccurrenceReclaimed,
            "trigger_route_unavailable" => Self::TriggerRouteUnavailable,
            "trigger_route_revoked" => Self::TriggerRouteRevoked,
            "process_no_longer_retained" => Self::ProcessNoLongerRetained,
            "process_registry_unavailable" => Self::ProcessRegistryUnavailable,
            "process_signal_wait_cancelled" => Self::ProcessSignalWaitCancelled,
            "process_signal_wait_handed_over" => Self::ProcessSignalWaitHandedOver,
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
            "engine_root_substrate_lost" => Self::EngineRootSubstrateLost,
            "engine_control_request" => Self::EngineControlRequest,
            "engine_control_unsupported" => Self::EngineControlUnsupported,
            "engine_handle_mismatch" => Self::EngineHandleMismatch,
            "process_not_parked" => Self::ProcessNotParked,
            "process_park_superseded" => Self::ProcessParkSuperseded,
            "obligation_key_mismatch" => Self::ObligationKeyMismatch,
            "obligation_row_invariant" => Self::ObligationRowInvariant,
            "obligation_attempt_budget_exceeded" => Self::ObligationAttemptBudgetExceeded,
            "obligation_ask_unadmitted" => Self::ObligationAskUnadmitted,
            "obligation_claim_lost" => Self::ObligationClaimLost,
            "session_delete_cleanup_pending" => Self::SessionDeleteCleanupPending,
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
            "runtime_effect_group_child_committed_final_lost" => {
                Self::RuntimeEffectGroupChildCommittedFinalLost
            }
            "runtime_effect_group_child_unroutable" => Self::RuntimeEffectGroupChildUnroutable,
            "runtime_effect_group_drain_deferred" => Self::RuntimeEffectGroupDrainDeferred,
            "runtime_effect_group_shape" => Self::RuntimeEffectGroupShape,
            "aggregate_await_unsettled" => Self::AggregateAwaitUnsettled,
            "max_tool_calls_exceeded" => Self::MaxToolCallsExceeded,
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
            "runtime_effect_tool_child_request_opener" => Self::RuntimeEffectToolChildRequestOpener,
            "usage_run_missing" => Self::UsageRunMissing,
            "usage_admission_fault" => Self::UsageAdmissionFault,
            "usage_owner_retired" => Self::UsageOwnerRetired,
            "runtime_effect_tool_child_request_version" => {
                Self::RuntimeEffectToolChildRequestVersion
            }
            "runtime_effect_tool_settlement_version" => Self::RuntimeEffectToolSettlementVersion,
            "runtime_effect_wrong_outcome" => Self::RuntimeEffectWrongOutcome,
            "runtime_effect_controller_task_closed" => Self::RuntimeEffectControllerTaskClosed,
            "writer_fenced" => Self::WriterFenced,
            "turn_cancel_binding_mismatch" => Self::TurnCancelBindingMismatch,
            "turn_cancel_closure_owner_released" => Self::TurnCancelClosureOwnerReleased,
            "store_incompatible" => Self::StoreIncompatible,
            "store_session_mismatch" => Self::StoreSessionMismatch,
            "runtime_store" => Self::RuntimeStore,
            "runtime_store_corrupt" => Self::RuntimeStoreCorrupt,
            "session_command_run" => Self::SessionCommandRun,
            "session_command_idempotency_key" => Self::SessionCommandIdempotencyKey,
            "session_command_post_drive_refresh" => Self::SessionCommandPostDriveRefresh,
            "session_command_refresh" => Self::SessionCommandRefresh,
            "session_command_refresh_tools" => Self::SessionCommandRefreshTools,
            "session_delete_scope_mismatch" => Self::SessionDeleteScopeMismatch,
            "session_head_refresh" => Self::SessionHeadRefresh,
            "session_tool_registry" => Self::SessionToolRegistry,
            "tool_catalog_resolution_failed" => Self::ToolCatalogResolutionFailed,
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
/// The identity of a submission refused for using another ingress kind's
/// reserved source-key namespace.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct IngressReservedSourceKeyRefusal {
    pub session_id: SessionId,
    pub ingress_kind: String,
    pub source_key: String,
}

/// The session-state generations an admission refused (FIG-3619): the one
/// the session's marker holds and the one this build admits.
///
/// A [`RuntimeError`] carries these fields in its typed store refusal, so
/// they survive plugin journaling and the engine's answer to the host.
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
    /// redrives the same turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_input_acceptance:
        Option<Box<crate::turn_input_vocabulary::TurnInputAcceptanceReceipt>>,
    /// The cause class the minting host chose for a foreign code (FIG-3575).
    /// Never persisted: read back from the wire, a foreign code is a recorded
    /// outcome.
    #[serde(skip)]
    foreign_cause: Option<TurnFailureCause>,
    /// The generations an executable-generation admission refused (FIG-3571).
    /// Never persisted.
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
            executable_generation_refusal: None,
        }
    }

    /// The typed refusal of an artifact publish or acquire under a referrer
    /// that has a fence.
    #[must_use]
    pub fn artifact_referrer_ended(referrer: crate::artifact_referrer::ArtifactReferrer) -> Self {
        let mut error = Self::new(
            RuntimeErrorCode::ArtifactReferrerEnded,
            format!("artifact referrer `{referrer}` has ended"),
        );
        error.cause = Some(RuntimeErrorCause::ArtifactReferrerEnded {
            referrer: Box::new(referrer),
        });
        error
    }

    /// The fenced referrer an `ArtifactReferrerEnded` refusal names. `None`
    /// on any other error.
    #[must_use]
    pub fn ended_referrer(&self) -> Option<&crate::artifact_referrer::ArtifactReferrer> {
        RuntimeErrorCause::ended_referrer(self.cause.as_ref())
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

    /// The typed refusal of a config transaction whose resolution a build
    /// would run with other reducers than it was admitted under (FIG-4379):
    /// an owner's reducer implementation is part of the executable identity
    /// an unresolved transaction binds. Its command root parks before any
    /// reducer runs, for a build that runs the admitted reducers.
    pub fn retired_config_reducer(owner: &str, recorded: &str, current: Option<&str>) -> Self {
        let current_spelling = current.unwrap_or("none");
        Self::refused_generation(
            ExecutableGenerationRefusal {
                found: Some(ExecutableGeneration::new(format!(
                    "config-owner:{owner}:{recorded}"
                ))),
                current: current.map(|current| {
                    ExecutableGeneration::new(format!("config-owner:{owner}:{current}"))
                }),
            },
            format!(
                "the config transaction was admitted under config owner `{owner}`'s reducer \
                 implementation {recorded}, and this build runs {current_spelling}: its \
                 resolution was refused before any reducer ran; resolve it under a build that \
                 runs {recorded}, or cancel it"
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

    /// The generations a session-state admission refused, on the error the
    /// refused call returned or read back from storage. `None` on any other
    /// error, including an older record that holds only a code and message.
    pub fn session_state_version_refusal(&self) -> Option<SessionStateVersionRefusal> {
        match self.store_refusal()? {
            crate::store::StoreRefusal::SessionStateVersionUnsupported { found, current }
            | crate::store::StoreRefusal::SessionStateVersionNewerThanRuntime { found, current } => {
                Some(SessionStateVersionRefusal {
                    found: *found,
                    current: *current,
                })
            }
            _ => None,
        }
    }

    /// The store refusal this error carries, when a store's typed refusal is
    /// its cause ([`StoreRefusal::of_store_error`](crate::store::StoreRefusal::of_store_error)).
    /// Every boundary that keeps a store refusal typed asks this, so a new
    /// refusal is recognised wherever one is.
    pub fn store_refusal(&self) -> Option<&crate::store::StoreRefusal> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::StoreRefusal { refusal } => Some(refusal),
            _ => None,
        }
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
            RuntimeErrorCause::VmWorker { .. }
            | RuntimeErrorCause::ArtifactReferrerEnded { .. }
            | RuntimeErrorCause::EffectGroupChildUnroutable { .. }
            | RuntimeErrorCause::IngressReservedSourceKey { .. }
            | RuntimeErrorCause::ModelUnavailable { .. }
            | RuntimeErrorCause::AttachmentRetention { .. }
            | RuntimeErrorCause::MaxToolCallsExceeded { .. }
            | RuntimeErrorCause::StoredDataCorrupt { .. }
            | RuntimeErrorCause::ModuleArtifactRefused { .. }
            | RuntimeErrorCause::RunShapeRefused { .. }
            | RuntimeErrorCause::ConfigRefused { .. }
            | RuntimeErrorCause::MissingRecordedProcessConfig { .. }
            | RuntimeErrorCause::StoreRefusal { .. }
            | RuntimeErrorCause::PluginFormat { .. }
            | RuntimeErrorCause::ProcessParentEnded { .. }
            | RuntimeErrorCause::ProcessStartKeyConflict { .. } => None,
        }
    }

    /// Whether retrying this exact failure is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        !self.has_terminal_cause() && self.code.is_retryable()
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
        self.has_terminal_cause()
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
pub enum EffectErrorJournalPolicy {
    #[default]
    Terminal,
    RetryUncommittedResponseDerivation,
}

impl EffectErrorJournalPolicy {
    pub fn is_retryable_derivation(self) -> bool {
        matches!(self, Self::RetryUncommittedResponseDerivation)
    }
}

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    #[serde(skip)]
    journal_disposition: EffectErrorJournalPolicy,
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
            journal_disposition: EffectErrorJournalPolicy::Terminal,
            code,
            message: message.into(),
            summary: None,
            cause: None,
            journaled: false,
            foreign_cause: None,
        }
    }

    /// A durable record cannot be decoded. Keeps its kind and codec diagnostic
    /// typed through the journal, plugin and runtime boundaries.
    #[must_use]
    pub fn stored_data_corrupt(record_kind: impl Into<String>, message: impl Into<String>) -> Self {
        let record_kind = record_kind.into();
        let message = message.into();
        let mut error = Self::new(
            RuntimeErrorCode::RuntimeStoreCorrupt,
            format!("stored {record_kind} data is corrupt: {message}"),
        );
        error.cause = Some(RuntimeErrorCause::StoredDataCorrupt {
            corruption: Box::new(StoredDataCorruption {
                record_kind,
                message,
            }),
        });
        error
    }

    /// The fenced referrer an `ArtifactReferrerEnded` refusal names. `None`
    /// on any other error.
    #[must_use]
    pub fn ended_referrer(&self) -> Option<&crate::artifact_referrer::ArtifactReferrer> {
        RuntimeErrorCause::ended_referrer(self.cause.as_ref())
    }

    /// Marks a failed, uncommitted host response derivation as safe to execute again.
    /// This authority is local to the executor; decoding a stored error cannot mint it.
    pub fn retryable_response_derivation(message: impl Into<String>) -> Self {
        let mut error = Self::new(
            RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
            message,
        );
        error.journal_disposition = EffectErrorJournalPolicy::RetryUncommittedResponseDerivation;
        error
    }

    /// Whether this failure is the attempt's own: an uncommitted derivation
    /// marked safe to execute again ([`Self::retryable_response_derivation`],
    /// [`Self::retryable_uncommitted_derivation`]). The retry runs the failed
    /// work again, so nothing may be journaled after it in its place — a
    /// cell that failed on its host's worker verdict records no cancellation
    /// peek either (FIG-4451).
    pub fn is_attempt_fault(&self) -> bool {
        !self.is_terminal() && self.journal_disposition.is_retryable_derivation()
    }

    /// Marks this failure of an uncommitted host derivation — an
    /// execution-environment sync's rebuild, or a recorded execution-environment
    /// load, or a presentation whose recorded renderer is unavailable — as
    /// safe to execute again. The claim is released unsealed instead of
    /// journaling the failure as the effect's outcome (FIG-3587, FIG-3683).
    ///
    /// A terminal cause or code never takes this authority (FIG-4629).
    /// Like session retirement (FIG-3630), it is a settled refusal, so the
    /// step records it and every replay decodes the same answer.
    #[must_use]
    pub fn retryable_uncommitted_derivation(mut self) -> Self {
        self.journal_disposition = if self.is_terminal() {
            EffectErrorJournalPolicy::Terminal
        } else {
            EffectErrorJournalPolicy::RetryUncommittedResponseDerivation
        };
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

    /// Only the host derivations — the before-LLM-call and assistant-response hooks,
    /// execution-environment sync, a checkpoint's store admission (FIG-4651),
    /// execution-environment load, presentation
    /// whose recorded renderer is unavailable, and a presentation or language
    /// value whose output retention faulted (FIG-1643) — and a
    /// drive's admission and seal, a root's resolution (its spec read and its
    /// definition lookup, FIG-3838), a config transaction's resolution under
    /// reducers other than it was admitted with (FIG-4379), a root's scope
    /// close and a session's close,
    /// whose store faults are the attempt's (FIG-3600), a trigger delivery's
    /// admission, whose binding read is the attempt's (FIG-4369), a follow-on
    /// recovery root's decision (FIG-4361), and a process command
    /// that marked its registry fault retryable (a session deletion's process
    /// cleanup, after its close) can consume derivation retry authority, as
    /// can any step whose cancellation watch was lost
    /// ([`Self::turn_cancel_watch_lost`]): that fault is about the attempt,
    /// never the step. A model call and a direct completion consume it for
    /// one more fault alone: the recorded model this worker could not bind
    /// before the call ([`Self::model_unavailable`], FIG-4404). A tool attempt
    /// also consumes local retry authority for a typed VM worker fault
    /// encountered before its pure derivation returned (FIG-4707).
    pub fn journal_disposition(&self, kind: RuntimeEffectKind) -> EffectErrorJournalPolicy {
        // The public cause can be attached after retry authority was granted.
        // No effect kind may consume that authority for a terminal refusal.
        if self.is_terminal() {
            return EffectErrorJournalPolicy::Terminal;
        }
        if matches!(
            kind,
            RuntimeEffectKind::BeforeLlmCall
                | RuntimeEffectKind::AssistantResponseHooks
                | RuntimeEffectKind::SyncExecutionEnvironment
                | RuntimeEffectKind::Checkpoint
                | RuntimeEffectKind::LoadExecutionEnv
                | RuntimeEffectKind::PresentToolResult
                | RuntimeEffectKind::LanguageRuntimeValue
                | RuntimeEffectKind::AdmitDrive
                | RuntimeEffectKind::SealDriveAdmission
                | RuntimeEffectKind::AdmitRoot
                | RuntimeEffectKind::InspectAdmittedHead
                | RuntimeEffectKind::RecoverFollowOn
                | RuntimeEffectKind::ResolveTurnConfig
                | RuntimeEffectKind::ResolveConfigTransaction
                | RuntimeEffectKind::CloseRootScope
                | RuntimeEffectKind::BeginSessionClose
                | RuntimeEffectKind::IngestTriggerOccurrence
                | RuntimeEffectKind::AdmitTriggerDelivery
                | RuntimeEffectKind::Process
        ) || self.code == RuntimeErrorCode::TransientCancelWatch
            || self.is_unbound_model_call(kind)
            || matches!(
                (kind, self.cause.as_ref()),
                (
                    RuntimeEffectKind::ToolAttempt,
                    Some(RuntimeErrorCause::VmWorker { .. })
                )
            )
        {
            self.journal_disposition
        } else {
            EffectErrorJournalPolicy::Terminal
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

    /// The store refusal this error carries
    /// ([`RuntimeError::store_refusal`]).
    pub fn store_refusal(&self) -> Option<&crate::store::StoreRefusal> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::StoreRefusal { refusal } => Some(refusal),
            _ => None,
        }
    }

    /// Whether retrying cannot succeed without a host-side change: a terminal
    /// cause, a foreign code its host minted as an outcome, or a terminal
    /// code.
    pub fn is_terminal(&self) -> bool {
        self.has_terminal_cause()
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
        if self.journaled || self.has_terminal_cause() {
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
            journal_disposition: EffectErrorJournalPolicy::Terminal,
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

impl RuntimeErrorCode {
    /// The code a store error is carried under past the store: the one
    /// mapping every boundary that classifies a store error reads, so a store
    /// error is terminal, or not, the same way on each of them.
    pub fn of_store_error(err: &crate::StoreError) -> Self {
        if let Some(refusal) = crate::store::StoreRefusal::of_store_error(err) {
            return refusal.code();
        }
        match err {
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
            crate::StoreError::ArtifactReferrerEnded { .. } => {
                crate::RuntimeErrorCode::ArtifactReferrerEnded
            }
            crate::StoreError::ArtifactMissing { .. } => crate::RuntimeErrorCode::ArtifactMissing,
            _ => crate::RuntimeErrorCode::RuntimeStore,
        }
    }
}

impl From<crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: crate::StoreError) -> Self {
        Self::from(&err)
    }
}

impl From<&crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: &crate::StoreError) -> Self {
        if let crate::StoreError::StoredDataCorrupt {
            record_kind,
            message,
        } = err
        {
            return Self::stored_data_corrupt(*record_kind, message.clone());
        }
        let refusal = crate::store::StoreRefusal::of_store_error(err);
        let cause = match err {
            crate::StoreError::SessionDeleted { session_id } => {
                Some(crate::RuntimeErrorCause::SessionDeleted {
                    session_id: session_id.clone(),
                })
            }
            crate::StoreError::ArtifactReferrerEnded { referrer } => {
                Some(crate::RuntimeErrorCause::ArtifactReferrerEnded {
                    referrer: Box::new(referrer.clone()),
                })
            }
            _ => None,
        };
        let cause = refusal
            .as_ref()
            .map(|refusal| RuntimeErrorCause::StoreRefusal {
                refusal: Box::new(refusal.clone()),
            })
            .or(cause);
        let code = RuntimeErrorCode::of_store_error(err);
        Self {
            journal_disposition: EffectErrorJournalPolicy::Terminal,
            code,
            message: err.to_string(),
            summary: None,
            cause,
            journaled: false,
            foreign_cause: None,
        }
    }
}

/// A generation read before a core bound it is a deployment fact no retry
/// changes: no generation lane is served yet.
impl From<crate::build_generation::GenerationUnbound> for RuntimeError {
    fn from(error: crate::build_generation::GenerationUnbound) -> Self {
        Self::new(
            RuntimeErrorCode::EngineServiceUnregistered,
            error.to_string(),
        )
    }
}
