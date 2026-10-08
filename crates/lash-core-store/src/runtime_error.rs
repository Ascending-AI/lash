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
mod controller;
mod recorded_refusal;
pub use cause::{RuntimeErrorCause, StoredDataCorruption};
pub use controller::RuntimeEffectControllerError;
pub use recorded_refusal::RecordedRefusal;
pub(crate) mod llm_profile_unavailable;
mod run_shape;
mod tool_call_limit;
pub(crate) use classification::RuntimeErrorClass;
pub use classification::TurnFailureCause;

/// Declare each built-in code, wire spelling and failure posture together.
/// Retired spellings name their canonical variant, so decoding cannot change
/// a recorded failure's posture after a semantic rename.
macro_rules! runtime_error_codes {
    (
        $(#[$meta:meta])* $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $wire:literal => $class:ident,)*
        }
        retired { $($retired:literal => $canonical:ident,)* }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        $vis enum $name {
            $($(#[$variant_meta])* $variant,)*
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

        impl $name {
            /// Every first-party code, derived from the declaration for law iteration.
            #[cfg(test)]
            pub(crate) const ALL_FIRST_PARTY: &[Self] = &[$(Self::$variant,)*];

            /// The canonical wire spelling; extensions keep their own spelling.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)*
                    Self::ForeignCode(code) => code.as_str(),
                }
            }

            /// Decode current and explicitly retired built-in spellings.
            /// Unknown extension strings remain recorded foreign outcomes.
            #[deny(unreachable_patterns)]
            pub fn from_wire_code(code: &str) -> Self {
                match code {
                    $($wire => Self::$variant,)*
                    $($retired => Self::$canonical,)*
                    other => Self::ForeignCode(other.to_string()),
                }
            }

            /// The code's one decided failure posture.
            pub(crate) const fn classification(&self) -> RuntimeErrorClass {
                match self {
                    $(Self::$variant => RuntimeErrorClass::$class,)*
                    Self::ForeignCode(_) => RuntimeErrorClass::Terminal,
                }
            }
        }
    };
}

runtime_error_codes! {
    /// Stable runtime error code.
    ///
    /// Codes serialize as the snake_case strings exposed in traces and host
    /// errors; callers match this type rather than parsing display text.
    pub enum RuntimeErrorCode {
        // the attachment policy judges the recorded attachment, so it refuses it again.
        AttachmentSourcePolicyDenied = "attachment_source_policy_denied" => Terminal,
        // a permanent fence already ended the referrer.
        /// An artifact publish or acquire named a referrer that has a fence
        /// (ADR 0113 §2.7). Store implementors return this code instead of
        /// wording the refusal as prose; the error carries the referrer.
        ArtifactReferrerEnded = "artifact_referrer_ended" => Terminal,
        // the bytes are not stored; a redrive reads the same state.
        /// An artifact acquire named bytes that are not stored.
        ArtifactMissing = "artifact_missing" => Terminal,
        // the bytes are not stored; a redrive reads the same state.
        ReferrerKindRefused = "referrer_kind_refused" => Terminal,
        // no referrer holds the descriptor; a redrive reads the same state.
        /// A process definition id names no stored descriptor: nothing holds it
        /// (ADR 0113 §3.6). A missing definition is never an empty success.
        DefinitionMissing = "definition_missing" => Terminal,
        // the engine judges the immutable descriptor, so it refuses it again.
        /// The owning engine refuses a stored or published definition: no engine
        /// of its kind, a value it cannot resolve, a forged signature claim, or a
        /// manifest that disagrees with the engine's resolution.
        DefinitionRefused = "definition_refused" => Terminal,
        // a worker becomes available without changing the operation.
        /// This host could not obtain a worker before the checkout deadline.
        /// No guest or definition verdict was produced; the attempt can retry.
        WorkerCheckoutTimedOut = "worker_checkout_timed_out" => Retryable,
        /// A worker fault whose typed cause distinguishes a retry from a run refusal.
        VmWorkerFailed = "vm_worker_failed" => Retryable,
        // A deployment repair serves the retained work; retries cannot repair it.
        /// This deployment cannot launch its configured worker. Repair and redrive.
        VmWorkerUnavailable = "vm_worker_unavailable" => Parked,
        // a contained panic of the effect body; the same body panics the same way.
        EffectPanicked = "effect_panicked" => Terminal,
        // the effect names no execution scope; wiring, not the attempt.
        MissingExecutionScopeId = "missing_execution_scope_id" => Terminal,
        // the scope names another turn; wiring, not the attempt.
        ExecutionScopeTurnIdMismatch = "execution_scope_turn_id_mismatch" => Terminal,
        // the scope lacks its incarnation; the identical admission fails identically.
        /// An execution scope was admitted for effect work without the process
        /// incarnation its scope kind requires (or pinned to an incarnation that
        /// is not its own). Retrying the identical admission fails identically;
        /// the caller must admit the scope through the authority that owns the
        /// process incarnation.
        ExecutionScopeAdmissionRefused = "execution_scope_admission_refused" => Terminal,
        // lease lost: a successor holds the lane, and a redrive under a new lease succeeds.
        SessionExecutionLeaseLost = "session_execution_lease_lost" => Redrivable,
        // a live foreign executor holds the lane; a later attempt takes it.
        /// A durable workflow controller's queued-work drain could not take the
        /// session execution lane: a live foreign executor holds it. Retrying the
        /// identical drain is explicitly safe, and pacing belongs to the engine's
        /// retry policy - the runtime deliberately stops waiting instead of
        /// blocking one invocation indefinitely.
        SessionExecutionLaneBusy = "session_execution_lane_busy" => Retryable,
        // the process-wide render pool drains; the identical composition then runs.
        /// A model call's prompt composition found the process-wide render
        /// pool's queue full (ADR 0133 §7). Nothing was admitted or sent, so
        /// the activation ends and its resume composes the call again.
        PromptRenderersBusy = "prompt_renderers_busy" => Retryable,
        // the park's redrive settles within a tick; the identical admission then proceeds.
        /// A session's shift admission found a turn input while the parked
        /// run's park still names a redrive intent that is not yet settled
        /// (D15). The input is refused rather than interleaved with the
        /// redrive; the refusal is never a recorded verdict, so the identical
        /// admission is safe to retry and admits the input once the redrive
        /// settles.
        SessionRedriveUnsettled = "session_redrive_unsettled" => Retryable,
        // the owning shift releases the head at its boundary; the identical write then lands.
        /// A head write outside every shift found the session head owned
        /// (FIG-4202): a bound run, an owed follow-on or an open session
        /// command. Nothing was written. The owner releases the head at its
        /// boundary, so the identical write is safe to retry then; a host moves
        /// the head through a session command instead.
        SessionHeadOwned = "session_head_owned" => Retryable,
        // the same direct call on a store-backed session is refused the same way.
        /// A host write that moves a store-backed session's head was called
        /// directly (FIG-4202): the bound turn owns the head, so the write is a
        /// session command the shift applies at a turn boundary. Submit it with
        /// `submit_session_command` and await its settlement; only a storeless
        /// runtime writes directly.
        SessionCommandRequired = "session_command_required" => Terminal,
        // the shift is journaled, so re-running the same turn cedes the same way.
        /// The journaled initial shift of a turn cannot execute the input that turn
        /// accepted: another claim of the live lease generation holds it; it is no
        /// longer open because it was settled, cancelled, or pruned by `vacuum()`;
        /// or, on a replay, a recovery drain reclaimed the journaled rows before the
        /// turn could commit them. Nothing is committed. The shift is a journaled
        /// effect (ADR 0069 §6), so re-running the same turn cedes the same way; the
        /// accepted input is answered, if at all, by the driver that holds or
        /// settled it.
        AcceptedTurnInputCeded = "accepted_turn_input_ceded" => Terminal,
        // the deployment runs no session work; the identical wait is refused identically.
        /// A caller waited on a session shift (`SessionWorkEngine::await_shift`)
        /// of a deployment that runs no session work: nothing will ever shift the
        /// session, so nothing will answer the wait. Configuring the core with a
        /// session-work engine is the recovery.
        SessionWorkUnavailable = "session_work_unavailable" => Terminal,
        // transactional write authority was contended; the identical commit is safe to retry.
        /// The store aborted a commit before publication because transactional
        /// write authority was contended. Retrying the same operation unchanged is
        /// safe; reloading or rebasing is not required.
        StoreCommitContended = "store_commit_contended" => Retryable,
        // the unfinished run is resumed by a later shift.
        /// Session work waits for an unfinished run.
        SessionRunPending = "session_run_pending" => Retryable,
        // a newer commit moved the head; a redrive reloads it and re-establishes authority.
        /// The final runtime commit lost the session-head compare-and-swap to a
        /// newer commit. Nothing from the losing commit was published, but the
        /// identical stale commit is not safe to retry: reload the durable head and
        /// re-establish current shift and run authority before building new work
        /// (ADR 0101).
        StoreCommitSuperseded = "store_commit_superseded" => Redrivable,
        // the session is gone.
        /// The session was deleted before its final runtime commit could publish.
        /// The session id is also retained in [`RuntimeErrorCause::SessionDeleted`]
        /// so hosts need not recover structured identity from display text.
        SessionDeleted = "session_deleted" => Terminal,
        // a capability fact about the deployment.
        /// The configured Session Catalog has no non-creating by-id lookup, so a
        /// consumer that strictly requires that seam can never resolve the
        /// session it names. A capability fact about the deployment, not a
        /// transient miss: retrying the identical lookup cannot change the
        /// answer, so this is terminal.
        SessionCatalogLookupUnsupported = "session_catalog_lookup_unsupported" => Terminal,
        // the deployment's config owners refused the recorded request.
        /// A session's config was refused at its creation: a namespace no
        /// installed owner registers, or a value its owner refuses (FIG-4379).
        /// The deployment's plugin set and the recorded request decide it, so
        /// retrying the identical creation cannot change the answer (FIG-4396).
        SessionConfigRefused = "session_config_refused" => Terminal,
        // the deployment's sources decide it; a run admitted on them is refused again.
        /// A turn run under `ToolSourcePolicy::Require` found a persisted Tool
        /// Catalog member no registered source resolves, and was refused
        /// before it installed the session's tool state (FIG-5134). Its
        /// typed half is
        /// [`RuntimeErrorCause::ToolSourcesUnavailable`], which carries the
        /// restore report.
        ToolSourcesUnavailable = "tool_sources_unavailable" => Terminal,
        // the row has no head; a redrive reads the same row.
        /// A session's catalog row has no head, so its creation recorded no
        /// config (FIG-4553). A session opens with the config its creation
        /// recorded and never with defaults, so the open is refused; a redrive
        /// reads the same row.
        SessionCreationUnrecorded = "session_creation_unrecorded" => Terminal,
        // the granted session is gone or never existed; its admission recorded that.
        /// A root process start holds a host session-lookup grant for a session
        /// the catalog does not hold live when the start's recorded admission
        /// runs. The admission records the refusal, so a replay answers it too.
        HostSessionNotLive = "host_session_not_live" => Terminal,
        // the session's generation marker is older than this build admits; a redrive reads the same marker.
        /// The session's durable state is an older generation than this build
        /// admits (FIG-3571, FIG-3619). Under the clean-cutover policy it is
        /// refused before any turn, model, tool or provider effect. A redrive on
        /// this build reads the same marker, so the refusal is terminal. The
        /// message names both generations; the error a refused call returns also
        /// carries them typed in
        /// [`RuntimeError::session_state_version_refusal`].
        SessionStateVersionUnsupported = "session_state_version_unsupported" => Terminal,
        // the session's generation marker is newer than this build knows; a redrive reads the same marker.
        /// The session's durable state is a newer generation than this build
        /// knows. Only a build that admits that generation can run it; the
        /// generations are carried as for
        /// [`Self::SessionStateVersionUnsupported`].
        SessionStateVersionNewerThanRuntime = "session_state_version_newer_than_runtime" => Terminal,
        // the same turn writes the same nodes and exceeds the budget again.
        /// The final runtime commit writes more graph and attachment-adoption rows
        /// than the shared node budget permits. The same turn will fail identically
        /// until the host produces a smaller turn.
        StoreCommitNodeBudgetExceeded = "store_commit_node_budget_exceeded" => Terminal,
        // the same turn writes the same bytes and exceeds the budget again.
        /// The final runtime commit contains more persisted payload bytes than the
        /// shared transaction budget permits. The same turn will fail identically
        /// until the host produces a smaller turn.
        StoreCommitByteBudgetExceeded = "store_commit_byte_budget_exceeded" => Terminal,
        // this build cannot read or write the codec version.
        /// A checkpoint component uses a codec version this build cannot read or
        /// write. The same commit cannot succeed until the store/session is
        /// recreated with a compatible Lash version.
        CheckpointComponentEncodingVersionMismatch = "checkpoint_component_encoding_version_mismatch" => Terminal,
        // serializing the same value with the same build fails the same way.
        /// A durable record failed deterministic serialization before publication.
        /// Retrying the same value with the same build cannot change the result.
        RecordEncodingFailed = "record_encoding_failed" => Terminal,
        // the run has no recorded policy; retrying cannot supply its missing authority.
        /// Terminal assembly has no installed run record from which to read
        /// its termination policy. Repair the run's recorded view before it
        /// can commit; the worker's live policy cannot replace that record.
        RecordedTerminationUnavailable = "recorded_termination_unavailable" => Terminal,
        // the process execution has no persisted id; wiring, not the attempt.
        /// A process (re-)execution was handed an empty/non-persisted process id.
        /// Process execution identity is the persisted `process_id`; a retry that
        /// cannot present that stable id has lost its idempotency anchor.
        MissingProcessExecutionId = "missing_process_execution_id" => Terminal,
        // live executor state could not be captured; nothing was published and a redrive recaptures it.
        /// Dirty executor state could not be captured before commit. No store
        /// publication was attempted; the live lease and claims are released.
        ExecutionStateCaptureFailed = "execution_state_capture_failed" => Redrivable,
        // resident state is invalidated in this process; a cold reopen from durable state repairs it.
        /// Resident plugin/protocol state was invalidated after a committed turn.
        /// Every subsequent resident-state consumer fails with this code until a
        /// durable reload succeeds. A deterministic restore fault therefore keeps
        /// returning this error; retry after repairing the cause or cold-open a new
        /// handle from the durable state.
        ResidentSessionReloadFailed = "resident_session_reload_failed" => Redrivable,
        // store I/O failed at commit.
        StoreCommitFailed = "store_commit_failed" => Redrivable,
        // a session-manager or registry write failed; store I/O, not the turn.
        PluginSessionManager = "plugin_session_manager" => Redrivable,
        // a plugin finalize hook refused over the same turn.
        PluginFinalizeTurn = "plugin_finalize_turn" => Terminal,
        // a plugin checkpoint hook refused over the same turn.
        PluginCheckpoint = "plugin_checkpoint" => Terminal,
        // a plugin prepare hook refused over the same inputs.
        PluginPrepareTurn = "plugin_prepare_turn" => Terminal,
        // context preparation over the same inputs fails the same way.
        ContextPrepareTurn = "context_prepare_turn" => Terminal,
        /// An administrative compaction failed before it opened its frame: its
        /// compactor, its prompt or its frame open refused (FIG-4201). The
        /// command that carried it settles with this code and is never applied
        /// again.
        ContextCompaction = "context_compaction" => Terminal,
        // the protocol refused the request before the model call; the same request is refused again.
        ProtocolBeforeLlmCall = "protocol_before_llm_call" => Terminal,
        // the facade's turn task died without reporting.
        TurnStreamJoin = "turn_stream_join" => Redrivable,
        // the agent-frame run produced no turn; the same run produces none again.
        EmptyAgentFrameRun = "empty_agent_frame_run" => Terminal,
        // a historical frame cannot become resident through this API; the switch is refused identically.
        /// A persisted historical frame cannot become resident through this API:
        /// switching it would replace resident configuration without a commanded
        /// config patch, and no such patch supports historical-frame switching.
        HistoricalAgentFrameSwitchUnsupported = "historical_agent_frame_switch_unsupported" => Terminal,
        // two authors named different switches; the identical turn conflicts identically.
        /// Two authors named a different agent-frame switch for one turn, or named
        /// the same frame with different seed nodes. A turn materializes at most
        /// one switch and there is no precedence order between its authors, so the
        /// commit is refused before any durable write. The identical turn fails
        /// identically until one of the two authors stops switching.
        AgentFrameSwitchAuthorConflict = "agent_frame_switch_author_conflict" => Terminal,
        // the host does not support await-event cancellation.
        AwaitEventCancelUnsupported = "await_event_cancel_unsupported" => Terminal,
        // signing the same key fails the same way.
        AwaitEventKeySign = "await_event_key_sign" => Terminal,
        // the await event is unknown or durably revoked.
        AwaitEventUnknownOrRevoked = "await_event_unknown_or_revoked" => Terminal,
        // the host does not support await events.
        AwaitEventUnsupported = "await_event_unsupported" => Terminal,
        // the cancel start gate was unavailable to this attempt; a retry reaches it.
        CancelStartGateUnavailable = "cancel_start_gate_unavailable" => Retryable,
        // the host cannot retire journals.
        EffectJournalRetirementUnsupported = "effect_journal_retirement_unsupported" => Terminal,
        // the scope is durably retired.
        EffectScopeRetired = "effect_scope_retired" => Terminal,
        // retirement was asked of a scope with live work; the same request is refused.
        EffectScopeNotQuiescent = "effect_scope_not_quiescent" => Terminal,
        // the scope's await events forbid retirement.
        AwaitEventScopeNotRetirable = "await_event_scope_not_retirable" => Terminal,
        // a malformed wait identity.
        InvalidAwaitEventWaitIdentity = "invalid_await_event_wait_identity" => Terminal,
        // a malformed cancel request.
        InvalidTurnCancelRequest = "invalid_turn_cancel_request" => Terminal,
        // the process-local live-replay buffer failed; nothing durable is involved.
        LiveReplay = "live_replay" => Redrivable,
        // the provider's failure is the recorded model-call result.
        LlmProvider = "llm_provider" => Terminal,
        // a refusal of the command that named the key; the same key is refused again.
        /// A send, a create request or a config command named a model key the
        /// host's registry does not register: a send and a create request are
        /// refused typed before anything is accepted, a config command is
        /// refused when its transaction resolves. Nothing changes either way.
        LlmProfileUnknown = "model_unknown" => Terminal,
        // the same selection over the same recorded capability is refused again.
        /// A send, a run's run spec or a child's create request selects
        /// reasoning the capability of the model it would run refuses
        /// (FIG-4531). It is refused where it is stated, before anything runs:
        /// the same selection over the same recorded capability is refused
        /// again.
        ReasoningRefused = "reasoning_refused" => Terminal,
        // the model was adopted when it was set; this worker's deployment cannot bind it now.
        /// A model a run recorded, or an admitted run's per-run key, has no
        /// binding on this worker. The model was adopted when it was set, so this
        /// is the worker's deployment, not the session's intent: the engine
        /// retries the run, and its retry budget parks it.
        LlmProfileUnavailable = "llm_profile_unavailable" => Retryable,
        // the recorded config selects no model; the same record is refused again.
        /// A recorded config selects no model, so the work it governs has
        /// nothing to run a model call with. It is a recorded absence no
        /// deployment can repair, so it is the work's outcome and is never
        /// retried, unlike [`Self::LlmProfileUnavailable`].
        LlmProfileUnconfigured = "llm_profile_unconfigured" => Terminal,
        // the spec names an exact revision this worker's deployment does not register yet.
        /// A run's run spec names a definition revision this worker does not
        /// register (FIG-3838). It is the deployment, not the input: the run
        /// retries, its retry budget parks it, and a redeploy that registers the
        /// revision recovers it. Nothing is recorded, and no other revision is
        /// ever used instead.
        RunDefinitionUnavailable = "run_definition_unavailable" => Retryable,
        /// A recorded renderer is absent on this worker. Redeploying it can resume the run.
        RecordedRendererUnavailable = "recorded_renderer_unavailable" => Retryable,
        // the attachment store refused or faulted; a healthy store retains the output.
        /// An output too long for history could not be retained as a session
        /// attachment before it entered history (FIG-1643). The output never
        /// enters history in its place: the step retries, and its retry budget
        /// parks the run.
        OutputRetentionFailed = "output_retention_failed" => Retryable,
        /// A required output retention was permanently refused by its attachment store.
        OutputRetentionRefused = "output_retention_refused" => Terminal,
        /// A canonical recorded result is unavailable or fails its material binding.
        RetainedResultRefused = "retained_result_refused" => Terminal,
        // a registered definition refuses the same context the same way.
        /// A registered run definition refused the spec's context (FIG-3838):
        /// deterministic, so it is recorded as the run's failure.
        RunShapeRefused = "run_shape_refused" => Terminal,
        // the same spec differs from the same running turn's again.
        /// An input addressed to a running turn carried an explicit run spec
        /// that differs from the turn's (FIG-3838): refused before acceptance.
        RunSpecMismatch = "run_spec_mismatch" => Terminal,
        // the same address names the same unknown turn again.
        /// An input addressed a turn that is neither its session's running turn
        /// nor one with its final commit recorded (ADR 0101 §5.1): refused before
        /// acceptance, with no row and no sequence number.
        TurnAddressUnknown = "turn_address_unknown" => Terminal,
        /// Admission refused a source key reserved for another ingress kind.
        IngressReservedSourceKey = "ingress_reserved_source_key" => Terminal,
        // a plugin refusal over the same inputs.
        Plugin = "plugin" => Terminal,
        // the selected queued work cannot be admitted; the same selection is refused again.
        QueuedWork = "queued_work" => Terminal,
        // one queued row alone exceeds the window; the same row exceeds it again.
        /// One queued row alone renders larger than the whole model context window,
        /// so no drain policy can make it fit and an automatic drain cannot execute
        /// it (FIG-1313). Retrying the identical drain fails identically until the
        /// row is cancelled or the window grows.
        QueuedWorkRowExceedsContextWindow = "queued_work_row_exceeds_context_window" => Terminal,
        // a contained panic of the process body; the same body panics the same way.
        ProcessPanicked = "process_panicked" => Terminal,
        // the target process is outside the visible set.
        /// ADR 0051 effect-host implementor diagnostic for a process-command
        /// refusal whose target is outside the invoking session's visible set.
        ProcessNotVisible = "process_not_visible" => Terminal,
        // a process runtime has no session; the same owner is refused again.
        /// A session-only operation was asked of a process runtime: a process
        /// has no session and no agent frame of its own.
        NotASessionRuntime = "not_a_session_runtime" => Terminal,
        // the target process is durably terminal.
        /// ADR 0051 effect-host implementor diagnostic for a write or cancellation
        /// refused because the recorded target is already terminal.
        ProcessAlreadyTerminal = "process_already_terminal" => Terminal,
        // the declared parent scope durably ended.
        /// Effect-host implementor diagnostic for a child registration refused
        /// because its declared parent scope has already ended.
        ProcessParentEnded = "process_parent_ended" => Terminal,
        // a different cancellation is durably recorded.
        /// Effect-host implementor diagnostic for a conflicting cancellation request.
        ProcessCancelConflict = "process_cancel_conflict" => Terminal,
        // the identity is durably bound to different content.
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
        DurableIdentityConflict = "durable_identity_conflict" => Terminal,
        // the host key is durably bound to another start.
        /// A host start key is bound to a retained process that another start
        /// made (ADR 0107): the retry presented a different start. The refusal
        /// names the key and nothing else, since the key is global and the
        /// retained process may be another originator's.
        ProcessStartKeyConflict = "process_start_key_conflict" => Terminal,
        /// A process cannot run without the configuration recorded at creation.
        MissingRecordedProcessConfig = "missing_recorded_process_config" => Terminal,
        // the key's family is fixed by how it was derived.
        /// A host rail was handed a start key of a family lash derives for its
        /// own start paths (ADR 0107): a host mints only host keys.
        StartKeyFamilyRefused = "start_key_family_refused" => Terminal,
        // the occurrence's tombstone outlives every redelivery it answers.
        /// An ingest named an occurrence retention reclaimed (FIG-4513).
        TriggerOccurrenceReclaimed = "trigger_occurrence_reclaimed" => Terminal,
        // the provider said nothing about the grant; the same delivery is asked again.
        /// A trigger delivery's start asked the host to restore its captured
        /// provider route, and the provider did not answer (FIG-4554). Nothing
        /// was recorded; the emission's retry asks again under the same
        /// identity.
        TriggerRouteUnavailable = "trigger_route_unavailable" => Retryable,
        // the provider withdrew the captured grant, and nothing re-resolves it.
        /// A trigger delivery's start asked the host to restore its captured
        /// provider route, and the provider revoked or refuses it (FIG-4554).
        /// Nothing registered, and nothing re-resolves the source.
        TriggerRouteRevoked = "trigger_route_revoked" => Terminal,
        // the target was replaced by a retention tombstone.
        /// ADR 0051 effect-host implementor diagnostic for a process-command
        /// refusal whose terminal target has been replaced by a retention tombstone.
        ProcessNoLongerRetained = "process_no_longer_retained" => Terminal,
        // a newer incarnation durably superseded this one.
        // no process execution is wired for this call.
        ProcessRegistryUnavailable = "process_registry_unavailable" => Terminal,
        // the durable signal wait settled cancelled.
        ProcessSignalWaitCancelled = "process_signal_wait_cancelled" => Terminal,
        // the wait moved to a successor segment; this segment ends at a
        // boundary, and no retry of it waits again.
        /// A process segment's signal wait was handed to a successor segment on
        /// the drain's wake (FIG-3799): the wait stays open and the body stops on
        /// it, for its continuation to wait again. Never a guest-visible outcome.
        ProcessSignalWaitHandedOver = "process_signal_wait_handed_over" => Terminal,
        // the durable signal wait settled timed out.
        ProcessSignalWaitTimeout = "process_signal_wait_timeout" => Terminal,
        // the wait moved to the Run's successor segment; this segment ends
        // at a boundary, and no retry of it waits again.
        /// A Run's durable wait was handed to the Run's successor segment on the
        /// drain's wake (FIG-4739): the wait stays open and the code cell that
        /// issued it stops on it, for its continuation to wait again. Never a
        /// guest-visible outcome.
        TurnWaitHandedOver = "turn_wait_handed_over" => Terminal,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventAwait = "engine_await_event_await" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventCancel = "engine_await_event_cancel" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventPeek = "engine_await_event_peek" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventResolve = "engine_await_event_resolve" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventRevocationRead = "engine_await_event_revocation_read" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventRevoke = "engine_await_event_revoke" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineAwaitEventSessionUpdate = "engine_await_event_session_update" => Retryable,
        // engine interaction failed; the engine redrives the invocation.
        EngineEffectController = "engine_effect_controller" => Redrivable,
        // the re-executed program no longer issues its recorded commands; only the build that wrote the journal serves it.
        /// A re-executed lashlang run — a code cell or a process body — issued a
        /// command that is not the one its journal recorded at that issue
        /// ordinal, or issued one while the journal still held entries at or
        /// beyond it (FIG-3586). Nothing was dispatched; the run stopped and the
        /// turn parks until an operator redeploys the build that wrote the
        /// journal, cancels, or forks.
        LashlangCellReplayDivergence = "lashlang_cell_replay_divergence" => Parked,
        // the turn was admitted under another executable generation; only a build of it serves it.
        /// A turn was redriven under another executable generation than the one
        /// its admission recorded (FIG-3571): the build running the redrive would
        /// compile, key or meter its cells differently from the build that wrote
        /// its journal, so it is refused at admission, before any effect, and the
        /// turn parks for a build of its own generation.
        RetiredGeneration = "retired_generation" => Parked,
        // a binding the cell's journal names moved; only its recorded results serve it.
        /// A redriven code cell needed a host tool binding its journaled binding
        /// set names, and the live tool for it is now missing or changed
        /// (FIG-3587). The binding is served only from recorded results: a call
        /// that would reach the live tool refuses, before anything is claimed.
        LashlangCellBindingDrift = "lashlang_cell_binding_drift" => Parked,
        // the turn was admitted under another executable generation; only a build of it serves it.
        /// Unfinished work requires a declared plugin revision this build cannot execute.
        PluginRevisionUnavailable = "plugin_revision_unavailable" => Parked,
        // the redrive diverged from the engine's journal; only the build that wrote it serves it.
        /// A redriven effect's reconstructed envelope differs from the one its
        /// engine journal recorded. The
        /// engine-neutral divergence code for journals the engine owns (the SQL
        /// hosts keep their store-qualified hash-conflict codes). Nothing was
        /// dispatched; the turn parks and the engine keeps the journal until an
        /// operator redeploys the build that wrote it, cancels, or forks.
        EffectReplayDivergence = "effect_replay_divergence" => Parked,
        // the host runs outside a handler scope; wiring, not the attempt.
        EngineEffectHostRequiresHandlerScope = "engine_effect_host_requires_handler_scope" => Terminal,
        // the give-up is journaled, so replay reproduces it.
        /// A recorded engine effect produced an unacceptable outcome and became
        /// terminal rather than failing every enclosing-turn redrive.
        EngineJournaledEffectPoisoned = "engine_journaled_effect_poisoned" => Terminal,
        // engine interaction failed; the engine redrives the invocation.
        EngineProcessAwait = "engine_process_await" => Redrivable,
        // engine interaction failed; the engine redrives the invocation.
        EngineProcessCancel = "engine_process_cancel" => Retryable,
        // the live command diverged from its journal entry; a redrive diverges the same way.
        /// A DirectProcess redrive addressed an existing engine record
        /// with a different canonical process-command identity.
        EngineProcessJournalIdentityDrift = "engine_process_journal_identity_drift" => Terminal,
        // the journal entry does not decode in this build.
        /// A DirectProcess engine record has an unsupported version or a
        /// shape this build cannot decode exactly.
        EngineProcessJournalPayloadIncompatible = "engine_process_journal_payload_incompatible" => Terminal,
        // the value's format stamp names a stored format this build does
        // not read; a redrive meets the same stamp.
        /// An engine object's retained state carries a stored-format stamp this
        /// build does not read — unstamped pre-format state, or a newer or
        /// skipped format; the engine refuses the value before any effect.
        EngineObjectStateFormatUnsupported = "engine_object_state_format_unsupported" => Terminal,
        // engine interaction failed; the engine redrives the invocation.
        EngineProcessIngressSubmit = "engine_process_ingress_submit" => Retryable,
        // a deployment fact: the service is not bound.
        /// The ingress target names an unbound service; retry cannot change that
        /// deployment fact, so this code is terminal.
        EngineServiceUnregistered = "engine_service_unregistered" => Terminal,
        // the turn is durably cancelled before the await.
        EngineProcessAwaitAfterTurnCancel = "engine_process_await_after_turn_cancel" => Terminal,
        // the cancel context is not wired.
        EngineProcessTurnCancelContextMissing = "engine_process_turn_cancel_context_missing" => Terminal,
        // the same terminal fails to encode again.
        EngineProcessTerminalEncode = "engine_process_terminal_encode" => Terminal,
        // engine interaction failed; re-attaching is safe.
        EngineTurnTerminalAttach = "engine_turn_terminal_attach" => Retryable,
        // the engine's control API did not carry out the ask; a later attempt asks again.
        /// A control verb or shift request did not reach the engine, or the engine
        /// did not carry it out. The refusal's disposition says whether asking
        /// again can succeed.
        EngineControlRequest = "engine_control_request" => Redrivable,
        // the installed engine has no such verb; only another engine changes that.
        /// The installed engine does not implement the control verb asked of it.
        EngineControlUnsupported = "engine_control_unsupported" => Terminal,
        /// A concurrent actor tried to register while the owner awaited a Run step.
        JournalWriteDuringOwnerStep = "journal_write_during_owner_step" => Terminal,
        // the process holds no park; a redrive of it has nothing to resume.
        /// A process redrive named a process that holds no park.
        ProcessNotParked = "process_not_parked" => Terminal,
        // the process parked again; the caller must act on the current park.
        /// A process redrive named a park the process has since replaced.
        ProcessParkSuperseded = "process_park_superseded" => Terminal,
        // the row lacks what its obligation needs; a retry reads the same row.
        /// The row an obligation lives on lacks the durable evidence its delivery
        /// needs, so the delivery can never be made as armed.
        ObligationRowInvariant = "obligation_row_invariant" => Terminal,
        // the attempt was cut at its budget; delivery is idempotent.
        /// One obligation delivery attempt ran past its kind's attempt budget and
        /// was abandoned.
        ObligationAttemptBudgetExceeded = "obligation_attempt_budget_exceeded" => Retryable,
        // the engine accepted the asks and admitted nothing; an operator re-arm asks again.
        /// The engine accepted every ask of a consumer-settled obligation and no
        /// consumer settled it.
        ObligationAskUnadmitted = "obligation_ask_unadmitted" => Redrivable,
        // another relay retook the claim; its attempt settles the row.
        /// A delivery's claim was retaken by another relay before the delivery
        /// settled what it owed.
        ObligationClaimLost = "obligation_claim_lost" => Retryable,
        // the cleanup the delete waits on is still running.
        /// A closing session's physical delete waits on cleanup that has not
        /// settled.
        SessionDeleteCleanupPending = "session_delete_cleanup_pending" => Retryable,
        // the attach ceiling elapsed; re-attaching is safe.
        /// An engine terminal attachment elapsed; re-attaching is safe.
        EngineTurnTerminalAttachCeilingElapsed = "engine_turn_terminal_attach_ceiling_elapsed" => Retryable,
        // the terminal does not decode.
        EngineTurnTerminalDecode = "engine_turn_terminal_decode" => Terminal,
        // the terminal resolution is invalid.
        EngineTurnTerminalInvalidResolution = "engine_turn_terminal_invalid_resolution" => Terminal,
        // the cancel names another scope; wiring, not the attempt.
        EngineTurnCancelScopeMismatch = "engine_turn_cancel_scope_mismatch" => Terminal,
        // the cancel names no scope; wiring, not the attempt.
        EngineTurnCancelScopeMissing = "engine_turn_cancel_scope_missing" => Terminal,
        // phase 2 re-derives over the durable completion, so a redrive repairs it.
        /// A journaled response hook retries derivation without paying again (FIG-1276).
        RuntimeEffectAssistantResponseHook = "runtime_effect_assistant_response_hook" => Retryable,
        // attachment store I/O failed before the effect ran.
        RuntimeEffectAttachmentStore = "runtime_effect_attachment_store" => Redrivable,
        // the envelope does not decode canonically.
        RuntimeEffectEnvelopeCanonicalDecode = "runtime_effect_envelope_canonical_decode" => Terminal,
        // the envelope breaks its hash invariant.
        RuntimeEffectEnvelopeCanonicalHashInvariant = "runtime_effect_envelope_canonical_hash_invariant" => Terminal,
        // hashing the same envelope fails the same way.
        RuntimeEffectEnvelopeHash = "runtime_effect_envelope_hash" => Terminal,
        // the owner can resume its cancelled logical aggregate wait.
        /// A cancelled logical aggregate wait can be resumed by its owner.
        RuntimeToolRunAwaitCancelled = "runtime_tool_run_await_cancelled" => Redrivable,
        // the logical owner durably cancelled before the new admission.
        /// The logical owner's durable cancellation already won, so a new
        /// completion or semantic admission is refused.
        RuntimeToolRunCancelDecided = "runtime_tool_run_cancel_decided" => Terminal,
        // native Run admission or receipt state is inconsistent.
        /// Native Run admission, material or receipt state is inconsistent.
        RuntimeToolRunShape = "runtime_tool_run_shape" => Terminal,
        // an awaited aggregate nothing can settle; the same program awaits it again.
        /// An awaited aggregate nothing can ever settle — `Promise.race([])`.
        /// ECMA-262 leaves such a promise pending forever; the host ends the
        /// execution with this typed failure instead of parking it, the analogue
        /// of Node exiting on an unsettled top-level await (ADR 0099 §11 clause 5,
        /// ADR 0062). A host lifetime contract, not a catchable exception.
        AggregateAwaitUnsettled = "aggregate_await_unsettled" => Terminal,
        // the recorded tool-call limit refuses the same call again.
        /// A tool call would take its cell or process past the session's
        /// recorded `max_tool_calls` (ADR 0099 §9, FIG-4546). Refused whole,
        /// before any child is dispatched; the typed half is
        /// [`RuntimeErrorCause::MaxToolCallsExceeded`]. The program's failure,
        /// not the host's: every replay refuses the same call.
        MaxToolCallsExceeded = "max_tool_calls_exceeded" => Terminal,
        // the invocation names an inconsistent subject.
        RuntimeEffectInvocationSubject = "runtime_effect_invocation_subject" => Terminal,
        // the effect names another scope; wiring, not the attempt.
        RuntimeEffectScopeMismatch = "runtime_effect_scope_mismatch" => Terminal,
        // the local executor does not match the command.
        RuntimeEffectLocalExecutorMismatch = "runtime_effect_local_executor_mismatch" => Terminal,
        // no local executor is wired for the command.
        RuntimeEffectLocalExecutorUnavailable = "runtime_effect_local_executor_unavailable" => Terminal,
        // the process-local effect task closed.
        RuntimeEffectLocalTaskClosed = "runtime_effect_local_task_closed" => Redrivable,
        // the process effect task died without reporting; nothing was recorded.
        RuntimeEffectProcessTaskJoin = "runtime_effect_process_task_join" => Redrivable,
        // the effect carries no replay key; wiring, not the attempt.
        RuntimeEffectReplayRequired = "runtime_effect_replay_required" => Terminal,
        // the sleep was cancelled in this process; nothing was recorded.
        RuntimeEffectSleepCancelled = "runtime_effect_sleep_cancelled" => Redrivable,
        // the effect task died without reporting; nothing was recorded.
        RuntimeEffectTaskJoin = "runtime_effect_task_join" => Redrivable,
        // the attempt index is inconsistent.
        RuntimeEffectToolAttemptIndex = "runtime_effect_tool_attempt_index" => Terminal,
        // the recorded presentation or execution environment cannot be reconstructed.
        ProcessExecutionEnvRefused = "process_execution_env_refused" => Terminal,
        // the effect produced an outcome of the wrong kind for its command.
        RuntimeEffectWrongOutcome = "runtime_effect_wrong_outcome" => Terminal,
        // the process-local controller task closed; a restart repairs it.
        /// Process-local; repaired by restart, not by same-process retry.
        RuntimeEffectControllerTaskClosed = "runtime_effect_controller_task_closed" => Redrivable,
        // the store refuses this build or this session on every attempt.
        /// A newer fleet epoch excludes this deployment's writable range.
        WriterFenced = "writer_fenced" => Terminal,
        // the store refuses this build or this session on every attempt.
        /// The store's compatibility stamp or fleet format cannot be admitted.
        StoreIncompatible = "store_incompatible" => Terminal,
        // the store refuses this build or this session on every attempt.
        /// A store returned state belonging to another session.
        StoreSessionMismatch = "store_session_mismatch" => Terminal,
        // the storage substrate faulted; the identical operation is safe to make again.
        RuntimeStore = "runtime_store" => Retryable,
        // durable state is corrupt or a counter is exhausted.
        /// Durable state is corrupt or an authoritative monotonic counter has
        /// exhausted its representable domain. Retrying unchanged cannot heal it.
        RuntimeStoreCorrupt = "runtime_store_corrupt" => Terminal,
        // the store answers the identical request the same way.
        /// The store's deterministic answer to a request, where the refusal has
        /// no code of its own: the identical request is refused the same way.
        StoreRefused = "store_refused" => Terminal,
        // the session command claim is refused.
        SessionCommandRun = "session_command_run" => Terminal,
        // the idempotency key is bound to a different command.
        SessionCommandIdempotencyKey = "session_command_idempotency_key" => Terminal,
        // the post-shift refresh read failed; a retry reads again.
        SessionCommandPostShiftRefresh = "session_command_post_shift_refresh" => Retryable,
        // the refresh read failed; a retry reads again.
        SessionCommandRefresh = "session_command_refresh" => Retryable,
        // the tool refresh failed; a retry refreshes again.
        SessionCommandRefreshTools = "session_command_refresh_tools" => Retryable,
        // the delete names another scope.
        SessionDeleteScopeMismatch = "session_delete_scope_mismatch" => Terminal,
        // the head refresh read failed; a retry reads again.
        SessionHeadRefresh = "session_head_refresh" => Redrivable,
        // the tool registry refuses the same registration.
        SessionToolRegistry = "session_tool_registry" => Terminal,
        // the same catalog resolves the same way.
        /// Process-local; repaired by restart, not by same-process retry.
        ToolCatalogResolutionFailed = "tool_catalog_resolution_failed" => Terminal,
        // the tool did not declare deferral.
        ToolDeferralNotDeclared = "tool_deferral_not_declared" => Terminal,
        // the cancel watch failed transiently.
        TransientCancelWatch = "transient_cancel_watch" => Retryable,
        // the cancel gate does not decode.
        TurnCancelGateDecode = "turn_cancel_gate_decode" => Terminal,
        // the same gate fails to encode again.
        TurnCancelGateEncode = "turn_cancel_gate_encode" => Terminal,
        // the cancel gate holds an invalid terminal.
        TurnCancelGateInvalidTerminal = "turn_cancel_gate_invalid_terminal" => Terminal,
        // the peeked turn-control outcome is invalid.
        TurnControlPeekOutcome = "turn_control_peek_outcome" => Terminal,
        // the turn control is unknown or durably revoked.
        TurnControlUnknownOrRevoked = "turn_control_unknown_or_revoked" => Terminal,
        // the local observer was cancelled while the durable promise stayed live.
        /// The local observer was cancelled while the durable promise stayed live;
        /// its disposition is unknown until another observer attaches.
        TurnControlWaitCancelled = "turn_control_wait_cancelled" => Redrivable,
        // the wait timed out in this process; the durable promise is untouched.
        TurnControlWaitTimeout = "turn_control_wait_timeout" => Retryable,
        // the terminal does not decode.
        TurnTerminalDecode = "turn_terminal_decode" => Terminal,
        // the same terminal fails to encode again.
        TurnTerminalEncode = "turn_terminal_encode" => Terminal,
        // the terminal resolution is invalid.
        TurnTerminalInvalidResolution = "turn_terminal_invalid_resolution" => Terminal,
        // the terminal is unknown or durably revoked.
        TurnTerminalUnknownOrRevoked = "turn_terminal_unknown_or_revoked" => Terminal,
        // no trigger store is wired.
        TriggerStoreUnavailable = "trigger_store_unavailable" => Terminal,
    }
    // No semantic renames are retained yet. Removed replacement-abort codes
    // remain foreign: they did not mean the current replay-divergence code.
    retired {}
}

/// A turn-input admission failure in the host's error vocabulary: the store
/// error's one classification ([`StoreError::runtime_error`](crate::StoreError::runtime_error)).
pub fn runtime_error_from_turn_input_admission(err: crate::store::StoreError) -> RuntimeError {
    err.runtime_error()
}

/// A commit failure in the host's error vocabulary: the store error's one
/// classification ([`StoreError::runtime_error`](crate::StoreError::runtime_error)).
pub fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    err.runtime_error()
}
impl RuntimeErrorCode {
    /// Whether this code reports that a replayed runtime effect diverged from
    /// the effect envelope recorded by its durable controller.
    ///
    /// Hosts use this predicate for alerting and drain policy.
    pub fn is_replay_mismatch(&self) -> bool {
        matches!(
            self,
            |Self::EngineProcessJournalIdentityDrift| Self::EffectReplayDivergence
                | Self::LashlangCellReplayDivergence
                | Self::RetiredGeneration
                | Self::PluginRevisionUnavailable
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
    pub fn deleted_session_id(&self) -> Option<&crate::SessionId> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::SessionDeleted { session_id } => Some(session_id),
            RuntimeErrorCause::ToolRunCutRefused { .. }
            | RuntimeErrorCause::ToolRunIsolationRefused { .. }
            | RuntimeErrorCause::ToolRunControl { .. }
            | RuntimeErrorCause::ToolRunAdmissionRefused { .. }
            | RuntimeErrorCause::MaterialRefused { .. }
            | RuntimeErrorCause::ProviderFailure { .. }
            | RuntimeErrorCause::VmWorker { .. }
            | RuntimeErrorCause::ArtifactReferrerEnded { .. }
            | RuntimeErrorCause::Compat { .. }
            | RuntimeErrorCause::IngressReservedSourceKey { .. }
            | RuntimeErrorCause::LlmProfileUnavailable { .. }
            | RuntimeErrorCause::AttachmentRetention { .. }
            | RuntimeErrorCause::MaxToolCallsExceeded { .. }
            | RuntimeErrorCause::StoredDataCorrupt { .. }
            | RuntimeErrorCause::ModuleArtifactRefused { .. }
            | RuntimeErrorCause::RunShapeRefused { .. }
            | RuntimeErrorCause::ConfigRefused { .. }
            | RuntimeErrorCause::ToolSourcesUnavailable { .. }
            | RuntimeErrorCause::MissingRecordedProcessConfig { .. }
            | RuntimeErrorCause::StoreRefusal { .. }
            | RuntimeErrorCause::PluginStateUnrecorded { .. }
            | RuntimeErrorCause::PluginStateEffectOwnerMismatch
            | RuntimeErrorCause::PluginStateFrontier { .. }
            | RuntimeErrorCause::PluginStatePublicationFenced { .. }
            | RuntimeErrorCause::PluginExecution { .. }
            | RuntimeErrorCause::PluginFormat { .. }
            | RuntimeErrorCause::PluginOperation { .. }
            | RuntimeErrorCause::PluginHooks { .. }
            | RuntimeErrorCause::ProcessParentEnded { .. }
            | RuntimeErrorCause::ProcessStartKeyConflict { .. }
            | RuntimeErrorCause::SchemaRefused { .. }
            | RuntimeErrorCause::ToolSchemaRefused { .. }
            | RuntimeErrorCause::ValueMismatch { .. } => None,
        }
    }

    /// Whether retrying this exact failure is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(RuntimeErrorCause::plugin_failure_class)
        {
            return class == lash_sansio::PluginFailureClass::Retryable;
        }
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
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(RuntimeErrorCause::plugin_failure_class)
        {
            return class == lash_sansio::PluginFailureClass::Terminal;
        }
        self.has_terminal_cause()
            || match self.foreign_cause {
                Some(cause) => cause == TurnFailureCause::Outcome,
                None => self.code.is_terminal(),
            }
    }

    /// The cause class of a turn this error fails (FIG-3575): an outcome
    /// exactly when the error is terminal.
    pub fn turn_failure_cause(&self) -> TurnFailureCause {
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(RuntimeErrorCause::plugin_failure_class)
        {
            return match class {
                lash_sansio::PluginFailureClass::Terminal => TurnFailureCause::Outcome,
                lash_sansio::PluginFailureClass::Parked => TurnFailureCause::Parked,
                _ => TurnFailureCause::LiveFault,
            };
        }
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

impl crate::store::DurableRecord for RuntimeErrorCode {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::compat::SQLITE_CORE_SCHEMA_VERSION);
}

#[cfg(test)]
mod declaration_tests {
    use super::RuntimeErrorClass;

    runtime_error_codes! {
        enum RenamedCode {
            Retry = "retry" => Retryable,
            Park = "park" => Parked,
        }
        retired {
            "old_retry" => Retry,
            "old_park" => Park,
        }
    }

    /// S19: a semantic rename keeps the recorded code's failure posture.
    #[test]
    fn retired_spellings_keep_the_canonical_classification() {
        for (old, current) in [
            ("old_retry", RenamedCode::Retry),
            ("old_park", RenamedCode::Park),
        ] {
            let decoded = RenamedCode::from_wire_code(old);
            assert_eq!(decoded, current);
            assert_eq!(decoded.classification(), current.classification());
            assert_eq!(decoded.as_str(), current.as_str());
        }
        for code in RenamedCode::ALL_FIRST_PARTY {
            assert_eq!(&RenamedCode::from_wire_code(code.as_str()), code);
        }
        assert_eq!(
            RenamedCode::from_wire_code("foreign:unknown").classification(),
            RuntimeErrorClass::Terminal
        );
    }
}
