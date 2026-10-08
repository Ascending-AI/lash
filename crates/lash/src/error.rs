use crate::support::SessionError;
use lash_sansio::SessionId;

#[derive(Debug, thiserror::Error)]
/// Errors returned while configuring or operating the embedded Lash runtime.
#[non_exhaustive]
pub enum EmbedError {
    /// A model call's admission record (its prompt snapshot, texts or exact body) could not be read.
    #[error(transparent)]
    PromptSnapshotLoad(#[from] lash_core::plugin::prompt::AdmittedCallLoadError),
    #[error(
        "protocol plugin is required; call .protocol_plugin(...) or use LashCore::standard_builder(backend)/LashCore::rlm_builder(backend, ...)"
    )]
    /// Returned when no protocol plugin was configured.
    MissingProtocolPlugin,
    #[error(
        "plugin `{plugin_id}` keeps its state in backend `{plugin_backend}`, but this core runs on backend `{backend}`; build the plugin over the core's own backend"
    )]
    /// Returned when a plugin factory bound to one backend's stores
    /// ([`PluginFactory::bound_backend`](lash_core::plugin::PluginFactory::bound_backend))
    /// is installed into a core over another backend: its state would live in
    /// a substrate the core neither reopens nor sweeps (ADR 0102, D2).
    PluginBackendMismatch {
        /// The factory's [`PluginFactory::id`](lash_core::plugin::PluginFactory::id).
        plugin_id: String,
        /// The binding identity of the backend the factory is bound to.
        plugin_backend: String,
        /// The binding identity of this core's backend.
        backend: String,
    },
    #[error(transparent)]
    /// Returned when a registered plugin factory's declaration is refused:
    /// it names another plugin than its factory, or cannot write the format
    /// it reads natively (FIG-4744). Nothing is built.
    PluginDeclaration(#[from] lash_core::plugin::PluginDeclarationError),
    #[error("a model key is required; a root session's spec must name a registered model")]
    /// Returned when a creation's spec states no model: an overlay
    /// passed where a
    /// root session is created. Nothing is created.
    MissingLlmProfile,
    #[error(transparent)]
    /// Returned when a creation's model key has no binding in the host's
    /// models. Nothing is created.
    LlmProfileUnknown(lash_core::LlmProfileUnavailable),
    #[error(transparent)]
    /// Returned when a creation spec records reasoning its model's recorded
    /// capability refuses. Nothing is created.
    ReasoningRefused(lash_core::ReasoningRefused),
    #[error(
        "turn budget is required; SessionSpec must carry TurnBudget::Bounded(...) or TurnBudget::Unbounded"
    )]
    /// Returned when a creation's spec states no turn budget: an overlay
    /// passed where a
    /// root session is created. Nothing is created.
    MissingTurnBudget,
    #[error(
        "max_tool_calls is required; SessionSpec must carry a MaxToolCalls: the total tool calls one cell may make, and the number a process may hold at once"
    )]
    /// Returned when the session has no explicit tool-call limit. There is
    /// no default and no built-in ceiling.
    MissingMaxToolCalls,
    /// The creator must choose a bound on consecutive unproductive attempts.
    #[error("no_progress_budget is required; choose a bound or explicit unbounded execution")]
    MissingNoProgressBudget,
    /// The host must choose how to handle a missing tool source.
    #[error("tool source policy is required; provide .tool_source_policy(...)")]
    MissingToolSourcePolicy,
    /// The host must state every execution bound the runtime enforces. They
    /// are its spend decision; the named preset is
    /// `ExecutionBudgets::recommended()`.
    #[error(
        "execution budgets are required; provide .execution_budgets(...), for example ExecutionBudgets::recommended()"
    )]
    MissingExecutionBudgets,
    /// The host must choose how stream deltas are coalesced, or that they
    /// are not.
    #[error(
        "delta coalescing is required; provide .delta_coalescing(...), for example DeltaCoalescing::recommended() or DeltaCoalescing::off()"
    )]
    MissingDeltaCoalescing,
    #[error(
        "a host session-turn start must state its session's {unstated}: build its create request with SessionCreateRequest::with_spec, or start it under a captured environment"
    )]
    /// Returned when a host starts a session-turn process whose create
    /// request does not state its session's whole config and whose start
    /// names no captured environment (FIG-4594). No session started it, so
    /// nothing recorded stands beneath it, and the core keeps no default.
    /// Nothing is registered.
    SessionTurnStartUnspecified {
        /// The part of the session's config the request left unstated.
        unstated: lash_core::UnstatedSessionConfig,
    },
    #[error(
        "commit budget is required; provide explicit byte and node limits with .commit_budget(...)"
    )]
    /// Returned when the runtime has no commit budget.
    MissingCommitBudget,
    #[error(
        "queued-work batching policy is required; provide an explicit model-action reserve with .queued_work_batching(...).tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)"
    )]
    /// Returned when queued-work batching has not been configured.
    MissingQueuedWorkBatching,
    #[error("session store operation failed: {0}")]
    Store(#[from] lash_core::StoreError),
    /// The durable store refused (ADR 0132): a fence, a mailbox rule or a
    /// store failure.
    #[error("durable store: {0}")]
    Durable(#[from] lash_core::durable_port::DurableError),
    #[error(
        "session `{session_id}` does not exist; only create() creates a session, so create it first with core.session(id).create(creation)"
    )]
    /// A verb other than [`create`](crate::SessionBuilder::create) named a
    /// session the catalog has never created. `open()`, `durable()`,
    /// `open_with_state()`, `observe_with_state()` and every Durable Session
    /// operation resolve an existing session and write no catalog row
    /// (FIG-4112).
    UnknownSession {
        /// Session identifier with no durable store.
        session_id: SessionId,
    },
    #[error(
        "session `{session_id}` already exists; create() never adopts an existing session, so open it with core.session(id).open()"
    )]
    /// [`create`](crate::SessionBuilder::create) named a session the catalog
    /// already holds. It is refused always — even when a retry states exactly
    /// the config the session recorded: the host owns its ids, and a host
    /// that means create-or-open treats this as present and opens.
    SessionAlreadyExists {
        /// Session identifier the catalog already holds.
        session_id: SessionId,
    },
    #[error(
        "session `{session_id}` has a catalog row and no head: its creation recorded no config, and a session never opens with defaults"
    )]
    /// The session's catalog row has no head, so its creation recorded no
    /// config (FIG-4553). A creating admission writes the creator's config
    /// with the row, and every open reads it back; a row without one is
    /// refused, never opened with defaults.
    SessionCreationUnrecorded {
        /// Session whose catalog row has no head.
        session_id: SessionId,
    },
    #[error("invalid work cadence: {0}")]
    WorkCadence(#[from] lash_core::WorkCadenceError),
    /// A ToolAdmin reconfiguration was refused before changing its catalog.
    #[error("tool reconfiguration: {0}")]
    Reconfigure(#[from] lash_core::facade_support::ReconfigureError),
    #[error(
        "session is still in use: park()/close() consume the session and require exclusive ownership; drop any cloned handles and finish or cancel in-flight turns first"
    )]
    /// Returned when an operation requires exclusive ownership of a session that is still in use.
    SessionStillInUse,
    #[error("failed to flush trace sink: {0}")]
    /// Wraps the trace flush failure.
    TraceFlush(#[from] lash_trace::TraceSinkError),
    #[error("runtime session error: {0}")]
    Session(#[source] SessionError),
    #[error("runtime turn error: {0}")]
    Runtime(#[from] lash_core::RuntimeError),
    /// A config transaction was not admitted (FIG-4379): it names an owner or
    /// command no installed plugin registers, its arguments do not decode, or
    /// its id was already submitted with other content. Nothing was enqueued.
    #[error("config transaction not admitted: {0}")]
    ConfigSubmit(lash_core::ConfigSubmitError),
    #[error("runtime plugin/control error: {0}")]
    Plugin(#[from] lash_core::PluginError),
    #[error("failed to encode protocol turn options: {0}")]
    ProtocolTurnOptions(#[from] serde_json::Error),
    #[error("failed to decode protocol turn options: {0}")]
    DecodeProtocolTurnOptions(#[from] lash_core::ProtocolTurnOptionsError),
    #[error("runtime control unavailable: {0}")]
    Control(#[from] lash_core::facade_support::PluginOperationInvokeError),
    /// A [`send`](crate::LashSession::send) or one of its handles could not
    /// answer (FIG-3600). Boxed: a [`SendError`] can carry a parked run's
    /// whole status, and every facade result carries this enum.
    #[error("send: {0}")]
    Send(Box<SendError>),
}

/// Blank text a host offers as an identity is the store's refusal of it.
impl From<lash_sansio::BlankIdentity> for EmbedError {
    fn from(error: lash_sansio::BlankIdentity) -> Self {
        Self::Store(error.into())
    }
}

impl From<SessionError> for EmbedError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::Store {
                source: lash_core::StoreError::SessionNotFound { session_id },
                ..
            } => Self::UnknownSession { session_id },
            error => Self::Session(error),
        }
    }
}

impl From<SendError> for EmbedError {
    fn from(error: SendError) -> Self {
        Self::Send(Box::new(error))
    }
}

impl EmbedError {
    /// The [`SendError`] this is, when it is one.
    pub fn send_error(&self) -> Option<&SendError> {
        match self {
            Self::Send(error) => Some(error),
            _ => None,
        }
    }
}

/// Why a [`send`](crate::LashSession::send), or a handle it returned, did not
/// answer with an outcome.
///
/// A committed turn that stopped (a provider error, max turns, a tool
/// failure) is not an error: it answers `Ok` with a
/// [`Failed`](crate::TurnStatus::Failed) status. A shift the engine refused
/// before the input settled surfaces as [`EmbedError::Runtime`] with the code
/// the refusal carried.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// [`output`](crate::SendHandle::output) was asked for the settled turn of
    /// an input that has none: its run parked, or the input was withdrawn
    /// before it ran. [`outcome`](crate::SendHandle::outcome) answers these
    /// without an error.
    #[error("input `{input_id}` has no settled turn: {status:?}")]
    NotSettled {
        /// The input the handle follows.
        input_id: lash_core::InputId,
        /// What the input's run answered instead.
        status: crate::TurnStatus,
    },
    /// The input was applied and its shift stopped, but no terminal for its
    /// run could be read within the handle's poll ceiling.
    #[error("input `{input_id}` settled but its terminal is unreadable")]
    Unresolved {
        /// The input the handle follows.
        input_id: lash_core::InputId,
    },
    /// The handle's live activity subscription fell outside the bounded
    /// replay window. The outcome is still readable from the store.
    #[error("observation gap: {0:?}")]
    ObservationGap(lash_core::facade_support::LiveReplayGap),
}

impl EmbedError {
    /// The acceptance of the direct turn this error aborted (FIG-3575).
    ///
    /// A direct turn that aborts after its input was durably accepted names
    /// the input here, so the host can stop it through
    /// [`DurableSession::attach`](crate::DurableSession::attach) and
    /// [`SendHandle::cancel`](crate::SendHandle::cancel).
    ///
    /// The receipt also comes back when the admitted turn already committed
    /// and a later turn of the same run aborted (an agent-frame follow-on
    /// turn). Cancellation follows the input's bound run and answers
    /// [`CancelReceipt::UnknownOrRevoked`](crate::CancelReceipt::UnknownOrRevoked)
    /// once that run has ended.
    pub fn turn_input_acceptance(&self) -> Option<&lash_core::runtime::TurnInputAcceptanceReceipt> {
        match self {
            Self::Runtime(err) => err.turn_input_acceptance.as_deref(),
            _ => None,
        }
    }

    /// The session-state generations the generation gate refused, when this
    /// is its refusal (FIG-3571, FIG-3619): at a session's open, at a turn's
    /// admission, or at a store call.
    ///
    /// The durable engine parks a turn still in flight that meets it on
    /// resume, rather than ending the turn (FIG-3735).
    pub fn session_state_version_refusal(&self) -> Option<lash_core::SessionStateVersionRefusal> {
        match self {
            Self::Store(error) | Self::Session(SessionError::Store { source: error, .. }) => {
                lash_core::SessionStateVersionRefusal::of_store_error(error)
            }
            Self::Runtime(error) => error.session_state_version_refusal(),
            Self::Plugin(lash_core::PluginError::Runtime(error)) => {
                error.session_state_version_refusal()
            }
            _ => None,
        }
    }

    /// Whether opening or writing the session met store contention.
    ///
    /// Unlike [`is_retryable`](Self::is_retryable), this excludes other
    /// transient storage faults and engine retries. Hosts can use it for a
    /// bounded admission retry without retrying unrelated failures.
    pub fn is_contended(&self) -> bool {
        matches!(
            self,
            Self::Store(lash_core::StoreError::Contended)
                | Self::Session(SessionError::Store {
                    source: lash_core::StoreError::Contended,
                    ..
                })
        )
    }

    /// True only when a typed signal says the failed operation is safe to
    /// retry as-is; `false` means "no typed retryable signal", not "known
    /// permanent" (see [`is_terminal`](Self::is_terminal) for that).
    ///
    /// Runtime failures delegate to the closed
    /// [`RuntimeErrorCode`](lash_core::RuntimeErrorCode) taxonomy. Its
    /// retryable set includes idempotent store contention, engine ingress,
    /// session-refresh, and bounded-wait operations, plus
    /// [`SessionExecutionLaneBusy`](lash_core::RuntimeErrorCode::SessionExecutionLaneBusy),
    /// the one Busy outcome that is a public runtime error: a durable workflow
    /// controller's queued drain hands lane contention back to its engine
    /// instead of blocking. Every other lease Busy stays an internal claim
    /// outcome. Foreign extension codes are conservatively not retryable.
    ///
    /// Notably
    /// [`SessionExecutionLeaseLost`](lash_core::RuntimeErrorCode::SessionExecutionLeaseLost)
    /// is non-retryable as-is: reload durable state and re-establish lease and
    /// claim authority before deciding whether to issue new work.
    ///
    /// [`StoreCommitFailed`](lash_core::RuntimeErrorCode::StoreCommitFailed)
    /// stays `false`: the code does not distinguish transient store I/O from
    /// conflicts, so there is no typed signal that a retry is safe.
    /// A direct or session-wrapped store error is retryable exactly when
    /// [`StoreError::is_transient`](lash_core::StoreError::is_transient) says
    /// the storage substrate faulted, as the engine's open retries it.
    ///
    /// Provider failures never surface as `EmbedError` — a failed LLM call
    /// finishes the turn with `TurnOutcome::Stopped(ProviderError)` — so
    /// their typed retryability is carried on
    /// [`TurnIssue::retryable`](crate::turn::TurnIssue) instead.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Runtime(err) => err.is_retryable(),
            Self::Control(err) => err.is_retryable(),
            Self::Plugin(err) | Self::Session(SessionError::Plugin(err)) => err.is_retryable(),
            Self::Reconfigure(_) => false,
            // A store error is retried here exactly when the engine retries
            // it: when it is a fault of the storage substrate.
            Self::Store(source) | Self::Session(SessionError::Store { source, .. }) => {
                source.is_transient()
            }
            // The runtime's `llm_profile_unavailable`, as a session error: a
            // deployment that serves the recorded key repairs it.
            Self::Session(SessionError::LlmProfileUnavailable { .. }) => true,
            // A busy or unreachable store, or a lost acknowledgement: the
            // same mailbox write is safe to repeat.
            Self::Durable(error) => durable_error_is_retryable(error),
            Self::PromptSnapshotLoad(error) => match error {
                lash_core::plugin::prompt::AdmittedCallLoadError::Store(error) => {
                    durable_error_is_retryable(error)
                }
                _ => false,
            },
            Self::MissingProtocolPlugin
            | Self::ConfigSubmit(_)
            | Self::PluginBackendMismatch { .. }
            | Self::PluginDeclaration(_)
            | Self::UnknownSession { .. }
            | Self::SessionAlreadyExists { .. }
            | Self::MissingLlmProfile
            | Self::LlmProfileUnknown(_)
            | Self::ReasoningRefused(_)
            | Self::MissingTurnBudget
            | Self::MissingMaxToolCalls
            | Self::MissingNoProgressBudget
            | Self::MissingToolSourcePolicy
            | Self::MissingExecutionBudgets
            | Self::MissingDeltaCoalescing
            | Self::SessionTurnStartUnspecified { .. }
            | Self::MissingCommitBudget
            | Self::MissingQueuedWorkBatching
            | Self::SessionCreationUnrecorded { .. }
            | Self::WorkCadence(_)
            | Self::SessionStillInUse
            | Self::TraceFlush(_)
            | Self::Session(_)
            | Self::ProtocolTurnOptions(_)
            | Self::DecodeProtocolTurnOptions(_)
            | Self::Send(_) => false,
        }
    }

    /// True only when a typed signal says retrying can never succeed without
    /// host-side changes (wiring, configuration, or invariant violations that
    /// a retry cannot repair). Errors that are neither
    /// [`is_retryable`](Self::is_retryable) nor terminal are simply unknown.
    ///
    /// The terminal set includes:
    ///
    /// - builder/wiring variants of this enum (missing protocol plugin,
    ///   default model key or an unknown one, turn budget, commit budget, queued-work composition,
    ///   handler context, and an obligation kind no relay can deliver) — the
    ///   same call fails
    ///   identically until the host changes its wiring;
    /// - typed runtime wiring, caller-invariant, unsupported-operation,
    ///   deterministic codec, and corrupt durable-state codes;
    /// - session model errors (`LlmProfileUnconfigured`, `LlmProfileUnknown`,
    ///   `CodeExecutionUnavailable`); a recorded model this deployment
    ///   cannot bind (`LlmProfileUnavailable`) is retryable instead, as the
    ///   runtime's `llm_profile_unavailable` is;
    /// - a direct or session-wrapped store error the engine carries under a
    ///   terminal code: every typed store refusal (compatibility, writer
    ///   fence, session identity, session-state generation, cancellation
    ///   authority), a tombstone, corrupt stored data, a commit byte or node
    ///   budget rejection, a checkpoint codec mismatch and a record-encoding
    ///   failure, plus a
    ///   [`StoreError::SessionRelationMismatch`](lash_core::StoreError::SessionRelationMismatch)
    ///   relation conflict and nested controller-owned terminal codes or
    ///   structured causes.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::MissingProtocolPlugin
            | Self::PluginBackendMismatch { .. }
            | Self::PluginDeclaration(_)
            | Self::MissingLlmProfile
            | Self::LlmProfileUnknown(_)
            | Self::ReasoningRefused(_)
            | Self::MissingTurnBudget
            | Self::MissingMaxToolCalls
            | Self::MissingNoProgressBudget
            | Self::MissingToolSourcePolicy
            | Self::MissingExecutionBudgets
            | Self::MissingDeltaCoalescing
            | Self::SessionTurnStartUnspecified { .. }
            | Self::MissingCommitBudget
            | Self::MissingQueuedWorkBatching
            | Self::SessionCreationUnrecorded { .. }
            | Self::UnknownSession { .. }
            | Self::SessionAlreadyExists { .. }
            | Self::ConfigSubmit(_) => true,
            Self::Send(_) => false,
            Self::Store(err) => store_error_is_terminal(err),
            Self::Durable(error) => durable_error_is_terminal(error),
            Self::PromptSnapshotLoad(error) => match error {
                lash_core::plugin::prompt::AdmittedCallLoadError::Store(error) => {
                    durable_error_is_terminal(error)
                }
                _ => true,
            },
            Self::Runtime(err) => err.is_terminal(),
            Self::Control(err) => err.is_terminal(),
            Self::Plugin(err) | Self::Session(SessionError::Plugin(err)) => err.is_terminal(),
            Self::Reconfigure(
                lash_core::facade_support::ReconfigureError::Validation(_)
                | lash_core::facade_support::ReconfigureError::UnknownSource(_),
            ) => true,
            Self::Reconfigure(
                lash_core::facade_support::ReconfigureError::GenerationMismatch { .. },
            ) => false,
            Self::Reconfigure(_) => false,
            Self::Session(SessionError::LlmProfileUnavailable { .. }) => false,
            Self::Session(SessionError::LlmProfileUnconfigured { .. })
            | Self::Session(SessionError::LlmProfileUnknown { .. })
            | Self::Session(SessionError::CodeExecutionUnavailable) => true,
            Self::Session(SessionError::Store { source, .. }) => store_error_is_terminal(source),
            Self::SessionStillInUse
            | Self::TraceFlush(_)
            | Self::ProtocolTurnOptions(_)
            | Self::DecodeProtocolTurnOptions(_)
            | Self::WorkCadence(_)
            | Self::Session(_) => false,
        }
    }
}

fn durable_error_is_retryable(error: &lash_core::durable_port::DurableError) -> bool {
    use lash_core::durable_port::{DurableError, StoreFailureKind};
    match error {
        DurableError::AckLost { .. } => true,
        DurableError::Store(failure) => matches!(
            failure.kind,
            StoreFailureKind::Contended | StoreFailureKind::Unavailable
        ),
        _ => false,
    }
}

/// A durable store that a newer release finalized, or a stored row that does
/// not decode, refuses every repeat.
fn durable_error_is_terminal(error: &lash_core::durable_port::DurableError) -> bool {
    use lash_core::durable_port::{DurableError, StoreFailureKind};
    matches!(
        error,
        DurableError::Store(failure)
            if matches!(failure.kind, StoreFailureKind::WriterRetired | StoreFailureKind::Corrupt)
    )
}

/// A store error is terminal at the facade exactly when the engine ends a
/// shift on it: the code the engine carries it under
/// ([`StoreError::runtime_code`](lash_core::StoreError::runtime_code)) is
/// terminal.
fn store_error_is_terminal(error: &lash_core::StoreError) -> bool {
    error.runtime_code().is_terminal()
}

/// Result type returned by Lash facade operations.
pub type Result<T> = std::result::Result<T, EmbedError>;

#[cfg(test)]
mod tests {
    use super::EmbedError;
    use lash_core::{
        PluginError, RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause,
        RuntimeErrorCode, SessionError, StoreError,
    };
    use lash_sansio::SessionId;

    fn runtime_error(code: RuntimeErrorCode) -> EmbedError {
        EmbedError::Runtime(RuntimeError::new(code, "test"))
    }

    #[test]
    fn head_ownership_stays_typed_and_recoverable_across_embed_boundary() {
        let session_id = SessionId::from("busy-session");
        for owner in [
            lash_core::store::SessionHeadOwner::Run {
                run: lash_core::TurnId::from("bound-run"),
            },
            lash_core::store::SessionHeadOwner::CommandLane { enqueue_seq: 7 },
        ] {
            let plugin = PluginError::from(StoreError::SessionHeadOwned {
                session_id: session_id.clone(),
                owner: owner.clone(),
            });
            for error in [
                EmbedError::from(plugin.clone()),
                EmbedError::from(SessionError::Plugin(plugin)),
            ] {
                assert!(error.is_retryable(), "{error}");
                assert!(!error.is_terminal(), "{error}");
                assert!(matches!(
                    error,
                    EmbedError::Plugin(PluginError::SessionHeadOwned {
                        session_id: found_session,
                        owner: found_owner,
                    }) | EmbedError::Session(SessionError::Plugin(PluginError::SessionHeadOwned {
                        session_id: found_session,
                        owner: found_owner,
                    })) if found_session == session_id && found_owner == owner
                ));
            }
        }
    }

    /// FIG-4531: a recorded model this deployment cannot bind is classified
    /// at the facade as the runtime classifies `llm_profile_unavailable`:
    /// retryable, never terminal. A key that was never registered stays
    /// terminal.
    #[test]
    fn an_unbindable_recorded_llm_profile_is_retryable_as_the_runtime_code_is() {
        let unavailable = || {
            lash_core::LlmProfileUnavailable::new(
                lash_core::LlmProfileKey::new("recorded-key"),
                lash_core::LlmProfileUnavailableReason::UnknownKey,
            )
        };
        let session_id = SessionId::from("unbindable");
        let session = EmbedError::Session(SessionError::LlmProfileUnavailable {
            session_id: session_id.clone(),
            source: unavailable(),
        });
        let runtime = runtime_error(RuntimeErrorCode::LlmProfileUnavailable);
        for error in [session, runtime] {
            assert!(error.is_retryable(), "{error}");
            assert!(!error.is_terminal(), "{error}");
        }
        let unknown = EmbedError::Session(SessionError::LlmProfileUnknown {
            session_id,
            source: unavailable(),
        });
        assert!(unknown.is_terminal() && !unknown.is_retryable());
    }

    #[test]
    fn controller_owned_lane_contention_stays_retryable_across_embed_boundary() {
        let error = EmbedError::Plugin(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::SessionExecutionLaneBusy,
                "durable controller lane is busy",
            ),
        ));

        assert!(error.is_retryable());
        assert!(!error.is_terminal());
    }

    #[test]
    fn terminal_controller_failure_stays_terminal_across_embed_boundary() {
        let error = EmbedError::Plugin(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectWrongOutcome,
                "durable controller returned the wrong outcome kind",
            ),
        ));

        assert!(!error.is_retryable());
        assert!(error.is_terminal());
    }

    #[test]
    fn session_execution_lease_lost_requires_reload_and_is_not_retryable_as_is() {
        let err = runtime_error(RuntimeErrorCode::SessionExecutionLeaseLost);
        assert!(!err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
    }

    #[test]
    fn store_commit_contended_is_retryable_and_not_terminal() {
        let errors = [
            runtime_error(RuntimeErrorCode::StoreCommitContended),
            EmbedError::Store(StoreError::Contended),
            EmbedError::Session(SessionError::Store {
                context: "failed to park a contended session".to_string(),
                source: StoreError::Contended,
            }),
            EmbedError::Plugin(PluginError::RuntimeEffectController(
                RuntimeEffectControllerError::from(StoreError::Contended),
            )),
        ];

        for error in errors {
            assert!(error.is_retryable(), "{error}");
            assert!(!error.is_terminal(), "{error}");
        }
    }

    #[test]
    fn contention_predicate_excludes_other_retryable_causes() {
        for error in [
            EmbedError::Store(StoreError::Contended),
            EmbedError::Session(SessionError::Store {
                context: "open session".to_string(),
                source: StoreError::Contended,
            }),
        ] {
            assert!(error.is_contended(), "{error}");
        }
        for error in [
            runtime_error(RuntimeErrorCode::StoreCommitContended),
            runtime_error(RuntimeErrorCode::SessionExecutionLaneBusy),
            EmbedError::Plugin(PluginError::RuntimeEffectController(
                RuntimeEffectControllerError::from(StoreError::Contended),
            )),
            EmbedError::UnknownSession {
                session_id: SessionId::from("unknown"),
            },
        ] {
            assert!(!error.is_contended(), "{error}");
        }
    }

    /// FIG-3575: a foreign code carries the class its minting host chose. A
    /// live fault is neither retryable nor terminal; an outcome, and a code
    /// read back from the wire as a recorded failure, is terminal.
    #[test]
    fn foreign_runtime_failures_follow_the_class_their_host_chose() {
        let live = EmbedError::Runtime(RuntimeError::foreign(
            "plugin_defined_crash",
            lash_core::TurnFailureCause::LiveFault,
            "test",
        ));
        assert!(!live.is_retryable(), "{live}");
        assert!(!live.is_terminal(), "{live}");
        for err in [
            EmbedError::Runtime(RuntimeError::foreign(
                "plugin_defined_abort",
                lash_core::TurnFailureCause::Outcome,
                "test",
            )),
            runtime_error(RuntimeErrorCode::from_wire_code("plugin_defined_abort")),
        ] {
            assert!(!err.is_retryable(), "{err}");
            assert!(err.is_terminal(), "{err}");
        }
    }

    #[test]
    fn unrelated_session_protocol_error_is_neither_retryable_nor_terminal() {
        // Guard against blanket-classifying Protocol as terminal to fix an
        // unrelated retry problem; only typed budget variants are terminal.
        let err = EmbedError::Session(SessionError::Protocol("unrelated failure".to_string()));
        assert!(!err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
    }

    #[test]
    fn wiring_errors_are_terminal_and_not_retryable() {
        for err in [
            EmbedError::MissingProtocolPlugin,
            EmbedError::PluginBackendMismatch {
                plugin_id: "plugin".to_string(),
                plugin_backend: "other".to_string(),
                backend: "backend".to_string(),
            },
            EmbedError::MissingTurnBudget,
            EmbedError::MissingMaxToolCalls,
            EmbedError::SessionTurnStartUnspecified {
                unstated: lash_core::UnstatedSessionConfig::Policy,
            },
            runtime_error(RuntimeErrorCode::MissingExecutionScopeId),
        ] {
            assert!(err.is_terminal(), "{err}");
            assert!(!err.is_retryable(), "{err}");
        }
    }

    #[test]
    fn commit_budget_errors_are_terminal_and_not_retryable() {
        for code in [
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            RuntimeErrorCode::StoreCommitByteBudgetExceeded,
        ] {
            let err = runtime_error(code);
            assert!(err.is_terminal(), "{err}");
            assert!(!err.is_retryable(), "{err}");
        }

        for err in [
            EmbedError::Store(StoreError::CommitNodeBudgetExceeded {
                node_count: 2,
                max_nodes: 1,
            }),
            EmbedError::Store(StoreError::CommitByteBudgetExceeded {
                session_config_bytes: 0,
                graph_delta_bytes: 2,
                checkpoint_bytes: 3,
                attachment_referrer_bytes: 5,
                turn_result_bytes: 0,
                total_bytes: 10,
                max_bytes: 9,
            }),
        ] {
            assert!(err.is_terminal(), "{err}");
            assert!(!err.is_retryable(), "{err}");
        }
    }

    #[test]
    fn deterministic_checkpoint_commit_errors_are_terminal_on_every_host_shape() {
        let runtime_errors = [
            RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch,
            RuntimeErrorCode::RecordEncodingFailed,
        ]
        .map(runtime_error);
        let session_errors = [
            StoreError::CheckpointComponentEncodingVersionMismatch {
                key: "execution_state".to_string(),
                actual: 2,
                expected: 1,
            },
            StoreError::RecordEncodingFailed {
                record_kind: "checkpoint root".to_string(),
                message: "deterministic fixture failure".to_string(),
            },
        ]
        .map(|source| {
            EmbedError::Session(SessionError::Store {
                context: "public append or park".to_string(),
                source,
            })
        });
        let direct_errors = [
            StoreError::CheckpointComponentEncodingVersionMismatch {
                key: "execution_state".to_string(),
                actual: 2,
                expected: 1,
            },
            StoreError::RecordEncodingFailed {
                record_kind: "checkpoint root".to_string(),
                message: "deterministic fixture failure".to_string(),
            },
        ]
        .map(EmbedError::Store);
        let plugin_errors = [
            StoreError::CheckpointComponentEncodingVersionMismatch {
                key: "execution_state".to_string(),
                actual: 2,
                expected: 1,
            },
            StoreError::RecordEncodingFailed {
                record_kind: "checkpoint root".to_string(),
                message: "deterministic fixture failure".to_string(),
            },
        ]
        .map(|source| {
            EmbedError::Plugin(PluginError::RuntimeEffectController(
                RuntimeEffectControllerError::from(source),
            ))
        });

        for error in runtime_errors
            .into_iter()
            .chain(direct_errors)
            .chain(session_errors)
            .chain(plugin_errors)
        {
            assert!(error.is_terminal(), "{error}");
            assert!(!error.is_retryable(), "{error}");
        }
    }

    #[test]
    fn superseded_commits_require_reload_across_every_host_shape() {
        let errors = [
            runtime_error(RuntimeErrorCode::StoreCommitSuperseded),
            EmbedError::Store(StoreError::HeadRevisionConflict {
                expected: 7,
                actual: 8,
            }),
            EmbedError::Session(SessionError::Store {
                context: "failed to park a superseded session".to_string(),
                source: StoreError::HeadRevisionConflict {
                    expected: 7,
                    actual: 8,
                },
            }),
            EmbedError::Plugin(PluginError::RuntimeEffectController(
                RuntimeEffectControllerError::from(StoreError::HeadRevisionConflict {
                    expected: 7,
                    actual: 8,
                }),
            )),
        ];

        for error in errors {
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.is_terminal(), "{error}");
        }
    }

    #[test]
    fn deleted_sessions_are_terminal_in_direct_and_wrapped_store_shapes() {
        let direct = EmbedError::Store(StoreError::SessionDeleted {
            session_id: SessionId::from("retired-direct"),
        });
        let wrapped = EmbedError::Session(SessionError::Store {
            context: "failed to bind retired session".to_string(),
            source: StoreError::SessionDeleted {
                session_id: SessionId::from("retired-wrapped"),
            },
        });
        let controller_owned = EmbedError::Runtime(
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "retired controller-owned session",
            )
            .with_cause(RuntimeErrorCause::SessionDeleted {
                session_id: SessionId::from("retired-controller-owned"),
            }),
        );
        let nested_controller_owned = EmbedError::Plugin(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::from(StoreError::SessionDeleted {
                session_id: SessionId::from("retired-nested-controller-owned"),
            }),
        ));

        for error in [direct, wrapped, controller_owned, nested_controller_owned] {
            assert!(error.is_terminal(), "{error}");
            assert!(!error.is_retryable(), "{error}");
        }
    }

    /// One of each store refusal that stays typed past the store.
    fn store_refusals() -> Vec<lash_core::store::StoreRefusal> {
        use lash_core::store::StoreRefusal;
        let refusals = vec![
            StoreRefusal::Incompatible {
                refusal: lash_core::compat::CompatRefusal::Unstamped {
                    component: "fleet_format".to_owned(),
                    writing_release: Some("facade-law".to_owned()),
                },
            },
            StoreRefusal::WriterFenced {
                recorded: 2,
                writable: lash_core::compat::VersionRange::exactly(1),
            },
            StoreRefusal::StoreSessionMismatch {
                loaded: SessionId::from("loaded"),
                requested: SessionId::from("requested"),
            },
            StoreRefusal::SessionStateVersionUnsupported {
                found: 0,
                current: 3,
            },
            StoreRefusal::SessionStateVersionNewerThanRuntime {
                found: 4,
                current: 3,
            },
        ];
        let codes = refusals
            .iter()
            .map(|refusal| refusal.code().as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(codes.len(), refusals.len(), "one sample per refusal");
        refusals
    }

    /// The direct and session-wrapped shapes a store error reaches a host in.
    fn store_shapes(error: impl Fn() -> StoreError) -> [EmbedError; 2] {
        [
            EmbedError::Store(error()),
            EmbedError::Session(SessionError::Store {
                context: "facade law".to_string(),
                source: error(),
            }),
        ]
    }

    /// The facade answers for a store error what the engine answers for the
    /// runtime error it turns that store error into (FIG-4628).
    #[test]
    fn the_facade_classifies_every_store_refusal_as_the_engine_does() {
        for refusal in store_refusals() {
            let engine = RuntimeEffectControllerError::from(refusal.clone().into_store_error())
                .into_runtime_error();
            assert_eq!(engine.code, refusal.code());
            assert!(engine.is_terminal() && !engine.is_retryable(), "{engine}");
            let shapes = store_shapes(|| refusal.clone().into_store_error())
                .into_iter()
                .chain([
                    EmbedError::Runtime(engine.clone()),
                    EmbedError::Plugin(PluginError::from(refusal.clone().into_store_error())),
                ]);
            for facade in shapes {
                assert_eq!(facade.is_terminal(), engine.is_terminal(), "{facade:?}");
                assert_eq!(facade.is_retryable(), engine.is_retryable(), "{facade:?}");
            }
        }
    }

    #[test]
    fn the_facade_retries_exactly_the_store_faults_the_engine_retries() {
        let transient: [fn() -> StoreError; 4] = [
            || StoreError::Contended,
            || StoreError::StorageFailure {
                backend: "facade-law",
                message: "the store is temporarily unavailable".to_string(),
            },
            || StoreError::Backend("the store is temporarily unavailable".to_string()),
            || StoreError::MigrationOpenElsewhere {
                database: "durable core".to_string(),
                location: std::path::PathBuf::from("facade-law"),
            },
        ];
        for error in transient {
            assert!(error().is_transient(), "{}", error());
            for facade in store_shapes(error) {
                assert!(facade.is_retryable() && !facade.is_terminal(), "{facade:?}");
            }
        }
        let corrupt = || StoreError::StoredDataCorrupt {
            record_kind: "SessionHeadMeta",
            message: "not the record".to_string(),
        };
        assert!(!corrupt().is_transient());
        for facade in store_shapes(corrupt) {
            assert!(facade.is_terminal() && !facade.is_retryable(), "{facade:?}");
        }
    }
}
