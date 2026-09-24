use crate::support::SessionError;
use lash_sansio::SessionId;

// The refusal vocabulary is core's own type, re-exported so a host never
// reaches into `lash_core` — the same shape ADR 0079 sanctions for the rest
// of the queue types at the crate root.
pub use lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause;

#[derive(Debug, thiserror::Error)]
/// Errors returned while configuring or operating the embedded Lash runtime.
#[non_exhaustive]
pub enum EmbedError {
    #[error(
        "protocol plugin is required; call .protocol_plugin(...) or use LashCore::standard_builder(backend, lash::TurnBudget::bounded(...))/LashCore::rlm_builder(backend, lash::TurnBudget::bounded(...), ...)"
    )]
    /// Returned when no protocol plugin was configured.
    MissingProtocolPlugin,
    #[error(
        "backend binding mismatch: the backend's binding identity is `{binding_identity}` but its effect host's turn-control binding is `{effect_host_binding}`; a backend's effect host must bind to the backend's own identity"
    )]
    /// Returned when a backend's effect host binds to an identity other than
    /// the backend's own, so its durable records would name a different
    /// substrate.
    BackendBindingMismatch {
        /// [`Backend::binding_identity`](lash_core::Backend::binding_identity).
        binding_identity: String,
        /// The effect host's `turn_control_binding_id()`.
        effect_host_binding: String,
    },
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
    #[error("model spec is required; hosts must supply explicit model metadata")]
    /// Returned when the session has no explicit model specification.
    MissingModelSpec,
    #[error(
        "turn budget is required; SessionSpec must carry TurnBudget::Bounded(...) or TurnBudget::Unbounded"
    )]
    /// Returned when the session has no explicit turn budget.
    MissingTurnBudget,
    #[error(
        "commit budget is required; provide explicit byte and node limits with .commit_budget(...)"
    )]
    /// Returned when the runtime has no commit budget.
    MissingCommitBudget,
    #[error(
        "queued-work batching policy is required; provide an explicit model-action reserve with .queued_work_batching(...)"
    )]
    /// Returned when queued-work batching has not been configured.
    MissingQueuedWorkBatching,
    #[error("failed to create store for session `{session_id}`: {message}")]
    StoreFactory {
        /// Session whose store could not be created.
        session_id: SessionId,
        message: String,
    },
    /// Session-store deletion stopped after witnessing some reclaim progress.
    ///
    /// The typed failure preserves the partial storage report required by ADR
    /// 0067 so hosts can distinguish witnessed progress from an empty scope.
    #[error("failed to delete store for session `{session_id}`: {failure}")]
    SessionDeleteStorage {
        /// Session whose durable storage deletion stopped.
        session_id: SessionId,
        /// Typed stop reason and the reclaim counters witnessed before it.
        failure: Box<lash_core::MaintenanceFailure<lash_core::SessionBlobReclaimReport>>,
    },
    #[error("session store operation failed: {0}")]
    Store(#[from] lash_core::StoreError),
    #[error(
        "session `{session_id}` has no durable session store; a Durable Session never creates one, so create the session first with core.session(id).open()"
    )]
    /// A Durable Session operation named a session the catalog has never
    /// created. Acquisition is deliberately non-creating, so this replaces the
    /// silent metadata materialisation an unknown-id enqueue once performed.
    UnknownSession {
        /// Session identifier with no durable store.
        session_id: SessionId,
    },
    #[error("store is bound to session `{loaded}` but builder requested `{requested}`")]
    /// A loaded store belongs to a different session than requested.
    StoreSessionMismatch {
        /// Session identifier bound to the loaded store.
        loaded: SessionId,
        /// Session identifier requested by the builder.
        requested: SessionId,
    },
    #[error("invalid process execution configuration: {0}")]
    ProcessExecutionConcurrency(#[from] lash_core_worker::ProcessExecutionConcurrencyError),
    #[error("invalid queued-work execution configuration: {0}")]
    QueuedWorkExecutionConcurrency(
        #[from] lash_core::facade_support::QueuedWorkExecutionConcurrencyError,
    ),
    #[error("invalid native substrate configuration: {0}")]
    NativeSubstrateConfig(#[from] lash_core::NativeSubstrateConfigError),
    #[error("failed to delete process state for session `{session_id}`: {message}")]
    /// Process-state deletion failed for the identified session.
    SessionDeleteProcess {
        /// Session whose process state could not be deleted.
        session_id: SessionId,
        /// Process-state deletion failure detail suitable for diagnostics.
        message: String,
    },
    #[error(
        "session is still in use: park()/close() consume the session and require exclusive ownership; drop any cloned handles and finish or cancel in-flight turns first"
    )]
    /// Returned when an operation requires exclusive ownership of a session that is still in use.
    SessionStillInUse,
    #[error("failed to flush trace sink: {0}")]
    /// Wraps the trace flush failure.
    TraceFlush(#[from] lash_trace::TraceSinkError),
    #[error(
        "pull-style turn streams require an effect host that can create a static scoped controller; use stream_to_with_effects(..., &controller) inside the handler context"
    )]
    /// Returned when a pull-style turn stream cannot obtain a static effect host.
    StaticTurnStreamRequiresStaticEffectHost,
    #[error("runtime session error: {0}")]
    Session(#[from] SessionError),
    #[error("selected queued-work drain refused: {cause:?}")]
    /// Wraps the selected queued work drain refused failure.
    SelectedQueuedWorkDrainRefused {
        /// Reason the selected queued-work drain was refused.
        cause: SelectedQueuedWorkDrainRefusalCause,
    },
    #[error("runtime turn error: {0}")]
    Runtime(#[from] lash_core::RuntimeError),
    #[error("runtime plugin/control error: {0}")]
    Plugin(#[from] lash_core::PluginError),
    #[error("remote protocol error: {0}")]
    RemoteProtocol(#[from] lash_remote_protocol::RemoteProtocolError),
    #[error("failed to encode protocol turn options: {0}")]
    ProtocolTurnOptions(#[from] serde_json::Error),
    #[error("failed to decode protocol turn options: {0}")]
    DecodeProtocolTurnOptions(#[from] lash_core::ProtocolTurnOptionsError),
    #[error("runtime control unavailable: {0}")]
    Control(#[from] lash_core::facade_support::PluginOperationInvokeError),
}

impl EmbedError {
    /// The acceptance of the direct turn this error aborted (FIG-3575).
    ///
    /// A direct turn that aborts after its input was durably accepted names
    /// the input here. The aborted turn keeps its claim on the input and binds
    /// it to its turn id before releasing its lease (FIG-3589), so no drain and
    /// no later direct turn folds the input into its own turn: the pending
    /// read reports it as
    /// [`TurnBound`](lash_core::PendingTurnInputReadStatus::TurnBound). The
    /// host decides its fate. Redrive the same turn id, which replays the
    /// aborted turn's journal and commits once, or withdraw the input by this
    /// receipt with
    /// [`DurableSession::cancel_pending_turn_input`](crate::DurableSession::cancel_pending_turn_input),
    /// which also returns any earlier inputs the aborted turn had absorbed to
    /// the queue. Those absorbed inputs cannot be cancelled on their own while
    /// bound: that cancel is refused as
    /// [`TurnBound`](lash_core::PendingTurnInputCancelOutcome::TurnBound).
    ///
    /// The journal was recorded against the session as that turn found it.
    /// Once a later turn commits, a redrive can no longer replay it and fails
    /// with a replay hash conflict
    /// ([`SqliteEffectReplayHashConflict`](lash_core::RuntimeErrorCode::SqliteEffectReplayHashConflict)
    /// or
    /// [`PostgresEffectReplayHashConflict`](lash_core::RuntimeErrorCode::PostgresEffectReplayHashConflict)),
    /// and the input stays bound: cancel it by this receipt.
    ///
    /// The binding is fenced by the aborted turn's claim. If the live fault was
    /// a lost lease and a successor generation had already re-claimed the input
    /// before the binding landed, the binding does nothing and that successor
    /// may fold the input into its own turn.
    ///
    /// The receipt also comes back when the admitted turn already committed
    /// and a later turn of the same run aborted (an agent-frame follow-on
    /// turn). The input is then settled, and a cancel by the receipt returns
    /// [`AlreadyCompleted`](lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted).
    pub fn turn_input_acceptance(&self) -> Option<&lash_core::runtime::TurnInputAcceptanceReceipt> {
        match self {
            Self::Runtime(err) => err.turn_input_acceptance.as_deref(),
            _ => None,
        }
    }

    /// True only when a typed signal says the failed operation is safe to
    /// retry as-is; `false` means "no typed retryable signal", not "known
    /// permanent" (see [`is_terminal`](Self::is_terminal) for that).
    ///
    /// Runtime failures delegate to the closed
    /// [`RuntimeErrorCode`](lash_core::RuntimeErrorCode) taxonomy. Its
    /// retryable set includes idempotent store contention, Restate ingress,
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
    /// Direct and session-wrapped [`StoreError::Contended`](lash_core::StoreError::Contended)
    /// are retryable for the same reason as the corresponding runtime code.
    /// A selected queued-work drain refused by
    /// [`SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy`] is likewise
    /// safe to retry unchanged; the other refusal causes require the host to
    /// reconsider the selection or input and remain unclassified.
    ///
    /// Provider failures never surface as `EmbedError` — a failed LLM call
    /// finishes the turn with `TurnOutcome::Stopped(ProviderError)` — so
    /// their typed retryability is carried on
    /// [`TurnIssue::retryable`](crate::turn::TurnIssue) instead.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Runtime(err) => err.is_retryable(),
            Self::Plugin(err) => err.is_retryable(),
            Self::Store(lash_core::StoreError::Contended)
            | Self::Session(SessionError::Store {
                source: lash_core::StoreError::Contended,
                ..
            })
            | Self::SelectedQueuedWorkDrainRefused {
                cause: SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy,
            } => true,
            Self::SelectedQueuedWorkDrainRefused {
                cause:
                    SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether { .. }
                    | SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                        ..
                    }
                    | SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { .. },
            }
            | Self::MissingProtocolPlugin
            | Self::BackendBindingMismatch { .. }
            | Self::PluginBackendMismatch { .. }
            | Self::UnknownSession { .. }
            | Self::MissingModelSpec
            | Self::MissingTurnBudget
            | Self::MissingCommitBudget
            | Self::MissingQueuedWorkBatching
            | Self::StoreFactory { .. }
            | Self::SessionDeleteStorage { .. }
            | Self::Store(_)
            | Self::StoreSessionMismatch { .. }
            | Self::ProcessExecutionConcurrency(_)
            | Self::QueuedWorkExecutionConcurrency(_)
            | Self::NativeSubstrateConfig(_)
            | Self::SessionDeleteProcess { .. }
            | Self::SessionStillInUse
            | Self::TraceFlush(_)
            | Self::StaticTurnStreamRequiresStaticEffectHost
            | Self::Session(_)
            | Self::RemoteProtocol(_)
            | Self::ProtocolTurnOptions(_)
            | Self::DecodeProtocolTurnOptions(_)
            | Self::Control(_) => false,
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
    ///   model spec, turn budget, commit budget, queued-work composition,
    ///   handler context, and store/session mismatches) — the same call fails
    ///   identically until the host changes its wiring;
    /// - typed runtime wiring, caller-invariant, unsupported-operation,
    ///   deterministic codec, and corrupt durable-state codes;
    /// - session provider-configuration errors (`ProviderMismatch`,
    ///   `ProviderUnconfigured`, `ProviderUnavailable`,
    ///   `CodeExecutionUnavailable`);
    /// - direct or session-wrapped
    ///   [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted)
    ///   tombstones and
    ///   [`StoreError::SessionRelationMismatch`](lash_core::StoreError::SessionRelationMismatch)
    ///   relation conflicts, plus nested controller-owned terminal codes or
    ///   structured causes;
    /// - direct or session-wrapped commit byte or node budget rejections, which
    ///   require the host to raise the configured limit or submit a smaller
    ///   commit;
    /// - direct or session-wrapped checkpoint codec mismatches and
    ///   record-encoding failures, which are deterministic for the same store
    ///   and build.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::MissingProtocolPlugin
            | Self::BackendBindingMismatch { .. }
            | Self::PluginBackendMismatch { .. }
            | Self::MissingModelSpec
            | Self::MissingTurnBudget
            | Self::MissingCommitBudget
            | Self::MissingQueuedWorkBatching
            | Self::StoreSessionMismatch { .. }
            | Self::ProcessExecutionConcurrency(_)
            | Self::QueuedWorkExecutionConcurrency(_)
            | Self::UnknownSession { .. }
            | Self::StaticTurnStreamRequiresStaticEffectHost => true,
            Self::Store(err) => store_error_is_terminal(err),
            Self::Runtime(err) => err.is_terminal(),
            Self::Plugin(err) => err.is_terminal(),
            Self::Session(SessionError::ProviderMismatch { .. })
            | Self::Session(SessionError::ProviderUnconfigured { .. })
            | Self::Session(SessionError::ProviderUnavailable { .. })
            | Self::Session(SessionError::CodeExecutionUnavailable) => true,
            Self::Session(SessionError::Store { source, .. }) => store_error_is_terminal(source),
            Self::StoreFactory { .. }
            | Self::SessionDeleteStorage { .. }
            | Self::SessionDeleteProcess { .. }
            | Self::SessionStillInUse
            | Self::TraceFlush(_)
            | Self::SelectedQueuedWorkDrainRefused {
                cause:
                    SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether { .. }
                    | SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                        ..
                    }
                    | SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy
                    | SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { .. },
            }
            | Self::RemoteProtocol(_)
            | Self::ProtocolTurnOptions(_)
            | Self::DecodeProtocolTurnOptions(_)
            | Self::Control(_)
            | Self::NativeSubstrateConfig(_)
            | Self::Session(_) => false,
        }
    }
}

fn store_error_is_terminal(error: &lash_core::StoreError) -> bool {
    matches!(
        error,
        lash_core::StoreError::SessionDeleted { .. }
            | lash_core::StoreError::SessionRelationMismatch { .. }
            | lash_core::StoreError::CommitNodeBudgetExceeded { .. }
            | lash_core::StoreError::CommitByteBudgetExceeded { .. }
            | lash_core::StoreError::CheckpointComponentEncodingVersionMismatch { .. }
            | lash_core::StoreError::RecordEncodingFailed { .. }
    )
}

/// Result type returned by Lash facade operations.
pub type Result<T> = std::result::Result<T, EmbedError>;

#[cfg(test)]
mod tests {
    use super::{EmbedError, SelectedQueuedWorkDrainRefusalCause};
    use lash_core::{
        PluginError, RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause,
        RuntimeErrorCode, SessionError, StoreError,
    };
    use lash_sansio::SessionId;

    fn runtime_error(code: RuntimeErrorCode) -> EmbedError {
        EmbedError::Runtime(RuntimeError::new(code, "test"))
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
    fn store_commit_failed_is_neither_retryable_nor_terminal() {
        let err = runtime_error(RuntimeErrorCode::StoreCommitFailed);
        assert!(!err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
    }

    #[test]
    fn execution_state_capture_failed_is_non_retryable_and_non_terminal() {
        let err = runtime_error(RuntimeErrorCode::ExecutionStateCaptureFailed);
        assert!(!err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
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
            EmbedError::BackendBindingMismatch {
                binding_identity: "backend".to_string(),
                effect_host_binding: "other".to_string(),
            },
            EmbedError::PluginBackendMismatch {
                plugin_id: "plugin".to_string(),
                plugin_backend: "other".to_string(),
                backend: "backend".to_string(),
            },
            EmbedError::MissingTurnBudget,
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
                attachment_manifest_bytes: 5,
                queue_batch_bytes: 0,
                agent_frame_bytes: 0,
                usage_delta_bytes: 0,
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
    fn selected_queued_work_drain_refusals_are_classified_per_cause() {
        let cases = [
            (
                SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                    unclaimed_batch_ids: vec!["unclaimable".to_string().into()],
                },
                false,
                false,
            ),
            (
                SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                    required_batch_ids: vec!["interrupted".to_string().into()],
                },
                false,
                false,
            ),
            (
                SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy,
                true,
                false,
            ),
            (
                SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
                    batch_id: "oversized".to_string().into(),
                    batch_enqueue_seq: 7,
                    required_context_tokens: 9,
                    max_context_tokens: 8,
                },
                false,
                false,
            ),
        ];

        for (cause, retryable, terminal) in cases {
            let error = EmbedError::SelectedQueuedWorkDrainRefused { cause };
            assert_eq!(error.is_retryable(), retryable, "{error}");
            assert_eq!(error.is_terminal(), terminal, "{error}");
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
}
