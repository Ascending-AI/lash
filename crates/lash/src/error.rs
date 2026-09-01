use crate::support::*;

/// Why a host-selected queued-work drain was refused before executing a turn.
///
/// Requested IDs with no remaining durable row are idempotently satisfied and
/// do not cause refusal. Each cause here means at least one still-present row
/// could not be executed under the requested atomic composition; no selected
/// turn was started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedQueuedWorkDrainRefusalCause {
    /// The requested present rows do not form one claimable queue composition.
    ///
    /// This includes physical queue gaps and mismatched batching gates. The
    /// attempted selected composition remains unexecuted.
    UnclaimableTogether {
        /// Requested batch IDs that the store could not claim with the rest.
        unclaimed_batch_ids: Vec<String>,
    },
    /// A requested row belongs to an interrupted claim whose complete,
    /// already-journaled composition must be redriven atomically. If a request
    /// partially covers more than one interrupted claim, this names the
    /// physically earliest incomplete claim in durable enqueue order.
    InterruptedBatchRequiresFullComposition {
        /// Complete interrupted composition, in durable enqueue order.
        required_batch_ids: Vec<String>,
    },
    /// Another host currently owns the session's execution lane while at least
    /// one requested row remains present.
    ExecutionLaneBusy,
    /// One requested row renders larger than the entire model context window.
    ///
    /// No drain policy can make such a row fit, so Lash names it and the window
    /// it would require rather than wedging the queue. Recovery is host policy:
    /// compact or split the work, or run the session on a larger-window model.
    QueuedItemExceedsContextWindow {
        /// Durable identity of the oversized row.
        batch_id: String,
        /// Durable queue position of the oversized row.
        batch_enqueue_seq: u64,
        /// Conservative tokens this row alone needs.
        required_context_tokens: usize,
        /// The session model's context window.
        max_context_tokens: usize,
    },
}

#[derive(Debug, thiserror::Error)]
/// Errors returned while configuring or operating the embedded Lash runtime.
pub enum EmbedError {
    #[error(
        "protocol plugin is required; call .protocol_plugin(...) or use LashCore::standard_builder(lash::TurnBudget::bounded(...))/LashCore::rlm_builder(lash::TurnBudget::bounded(...), ...)"
    )]
    /// Returned when no protocol plugin was configured.
    MissingProtocolPlugin,
    #[error("model spec is required; hosts must supply explicit model metadata")]
    /// Returned when the session has no explicit model specification.
    MissingModelSpec,
    #[error(
        "turn budget is required; SessionSpec must carry TurnBudget::Bounded(...) or TurnBudget::Unbounded"
    )]
    /// Returned when the session has no explicit turn budget.
    MissingTurnBudget,
    #[error("effect host is required; provide an explicit effect host with .effect_host(...)")]
    /// Returned when the runtime has no effect host.
    MissingEffectHost,
    #[error(
        "attachment store is required; provide an explicit attachment store with .attachment_store(...)"
    )]
    /// Returned when the runtime has no attachment store.
    MissingAttachmentStore,
    #[error(
        "process execution environment store is required; provide an explicit process env store with .process_env_store(...)"
    )]
    /// Returned when the runtime has no process-environment store.
    MissingProcessEnvStore,
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
    #[error(
        "runtime host config conflicts with builder field `{field}`; configure this value in exactly one place"
    )]
    /// Returned when a whole runtime host config and a duplicated builder
    /// field were both supplied.
    RuntimeHostConfigConflict {
        /// Builder field that duplicated a value in the runtime host config.
        field: &'static str,
    },
    #[error(
        "queued-work composition is required; call .with_native_queued_work(), .with_queued_work(...), or .without_queued_work()"
    )]
    /// Returned when the host did not choose how queued work is executed.
    MissingQueuedWorkSource,
    #[error(
        "native queued work requires a LashCore store factory; call .store_factory(...) or choose .with_queued_work(...) or .without_queued_work()"
    )]
    /// Returned when native queued work cannot rebuild session runtimes.
    NativeQueuedWorkRequiresStoreFactory,
    #[error("failed to create store for session `{session_id}`: {message}")]
    /// Store creation failed for the identified session.
    StoreFactory {
        /// Session whose store could not be created.
        session_id: String,
        /// Store-factory failure detail suitable for diagnostics.
        message: String,
    },
    /// Session-store deletion stopped after witnessing some reclaim progress.
    ///
    /// The typed failure preserves the partial storage report required by ADR
    /// 0067 so hosts can distinguish witnessed progress from an empty scope.
    #[error("failed to delete store for session `{session_id}`: {failure}")]
    SessionDeleteStorage {
        /// Session whose durable storage deletion stopped.
        session_id: String,
        /// Typed stop reason and the reclaim counters witnessed before it.
        failure: Box<lash_core::MaintenanceFailure<lash_core::SessionBlobReclaimReport>>,
    },
    #[error("session store operation failed: {0}")]
    /// Wraps the store failure.
    Store(#[from] lash_core::StoreError),
    #[error(
        "store-less session id `{session_id}` was already used by this LashCore; store-less sessions require distinct ids per process"
    )]
    /// A store-less session identifier was reused by this core.
    EphemeralSessionIdReused {
        /// Store-less session identifier that was reused.
        session_id: String,
    },
    #[error("store is bound to session `{loaded}` but builder requested `{requested}`")]
    /// A loaded store belongs to a different session than requested.
    StoreSessionMismatch {
        /// Session identifier bound to the loaded store.
        loaded: String,
        /// Session identifier requested by the builder.
        requested: String,
    },
    #[error("durable process worker requires a LashCore store factory")]
    /// Returned when a durable process worker has no store factory.
    MissingProcessWorkerStoreFactory,
    #[error(
        "a process registry is configured for the default native process work runner but no session store factory is wired; the runner rebuilds a session runtime per process and cannot do so without one. Wire .store_factory(...) - InMemorySessionStoreFactory::new() for ephemeral process execution, or a durable factory - or use .process_work(...) for an externally driven durable runner."
    )]
    /// Returned when the native process runner cannot rebuild sessions without a store factory.
    ProcessRegistryRequiresStoreFactory,
    #[error("durable process worker config requires a LashCore process registry")]
    /// Returned when durable process-worker configuration has no process registry.
    MissingProcessRegistry,
    #[error("invalid process execution configuration: {0}")]
    /// Wraps the process execution concurrency failure.
    ProcessExecutionConcurrency(
        #[from] lash_core::facade_support::ProcessExecutionConcurrencyError,
    ),
    #[error("invalid queued-work execution configuration: {0}")]
    /// Wraps the queued work execution concurrency failure.
    QueuedWorkExecutionConcurrency(
        #[from] lash_core::facade_support::QueuedWorkExecutionConcurrencyError,
    ),
    #[error("invalid native substrate configuration: {0}")]
    /// Wraps a native scheduler pacing validation failure.
    NativeSubstrateConfig(#[from] lash_core::NativeSubstrateConfigError),
    #[error("this operation requires a LashCore store factory")]
    /// Returned when an operation requiring durable session state has no store factory.
    MissingSessionStoreFactory,
    #[error("failed to delete process state for session `{session_id}`: {message}")]
    /// Process-state deletion failed for the identified session.
    SessionDeleteProcess {
        /// Session whose process state could not be deleted.
        session_id: String,
        /// Process-state deletion failure detail suitable for diagnostics.
        message: String,
    },
    #[error("missing required turn input for plugin `{plugin_id}`")]
    /// A plugin did not receive its required turn input.
    MissingPluginTurnInput {
        /// Identifier of the plugin whose required turn input is absent.
        plugin_id: &'static str,
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
        "pull-style turn streams require an effect host that can create a static scoped controller; use stream_to(...) inside the handler context"
    )]
    /// Returned when a pull-style turn stream cannot obtain a static effect host.
    StaticTurnStreamRequiresStaticEffectHost,
    #[error("runtime session error: {0}")]
    /// Wraps the session failure.
    Session(#[from] SessionError),
    #[error("selected queued-work drain refused: {cause:?}")]
    /// Wraps the selected queued work drain refused failure.
    SelectedQueuedWorkDrainRefused {
        /// Reason the selected queued-work drain was refused.
        cause: SelectedQueuedWorkDrainRefusalCause,
    },
    #[error("runtime turn error: {0}")]
    /// Wraps the runtime failure.
    Runtime(#[from] lash_core::RuntimeError),
    #[error("runtime plugin/control error: {0}")]
    /// Wraps the plugin failure.
    Plugin(#[from] lash_core::PluginError),
    #[error("remote protocol error: {0}")]
    /// Wraps the remote protocol failure.
    RemoteProtocol(#[from] lash_remote_protocol::RemoteProtocolError),
    #[error("failed to encode protocol turn options: {0}")]
    /// Wraps the protocol turn options failure.
    ProtocolTurnOptions(#[from] serde_json::Error),
    #[error("failed to decode protocol turn options: {0}")]
    /// Wraps the decode protocol turn options failure.
    DecodeProtocolTurnOptions(#[from] lash_core::ProtocolTurnOptionsError),
    #[error("runtime control unavailable: {0}")]
    /// Wraps the control failure.
    Control(#[from] lash_core::facade_support::PluginOperationInvokeError),
}

impl EmbedError {
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
    ///
    /// Provider failures never surface as `EmbedError` — a failed LLM call
    /// finishes the turn with `TurnOutcome::Stopped(ProviderError)` — so
    /// their typed retryability is carried on
    /// [`TurnIssue::retryable`](crate::turn::TurnIssue) instead.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Runtime(err) => err.is_retryable(),
            Self::Plugin(err) => err.is_retryable(),
            _ => false,
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
    ///   model spec, effect host, stores, registries, handler context, and
    ///   store/session mismatches) — the same call fails identically until
    ///   the host changes its wiring;
    /// - typed runtime wiring, caller-invariant, unsupported-operation,
    ///   deterministic codec, and corrupt durable-state codes;
    /// - session provider-configuration errors (`ProviderMismatch`,
    ///   `ProviderUnconfigured`, `ProviderUnavailable`,
    ///   `CodeExecutionUnavailable`);
    /// - direct or session-wrapped
    ///   [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted)
    ///   tombstones, plus nested controller-owned terminal codes or structured
    ///   causes;
    /// - direct or session-wrapped commit byte or node budget rejections, which
    ///   require the host to raise the configured limit or submit a smaller
    ///   commit;
    /// - session-wrapped checkpoint codec mismatches and record-encoding
    ///   failures, which are deterministic for the same store and build.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::MissingProtocolPlugin
            | Self::MissingModelSpec
            | Self::MissingEffectHost
            | Self::MissingAttachmentStore
            | Self::MissingProcessEnvStore
            | Self::MissingCommitBudget
            | Self::MissingQueuedWorkBatching
            | Self::RuntimeHostConfigConflict { .. }
            | Self::MissingQueuedWorkSource
            | Self::NativeQueuedWorkRequiresStoreFactory
            | Self::StoreSessionMismatch { .. }
            | Self::MissingProcessWorkerStoreFactory
            | Self::ProcessRegistryRequiresStoreFactory
            | Self::MissingProcessRegistry
            | Self::ProcessExecutionConcurrency(_)
            | Self::QueuedWorkExecutionConcurrency(_)
            | Self::MissingSessionStoreFactory
            | Self::MissingPluginTurnInput { .. }
            | Self::StaticTurnStreamRequiresStaticEffectHost => true,
            Self::Store(
                lash_core::StoreError::SessionDeleted { .. }
                | lash_core::StoreError::CommitNodeBudgetExceeded { .. }
                | lash_core::StoreError::CommitByteBudgetExceeded { .. },
            ) => true,
            Self::Runtime(err) => err.is_terminal(),
            Self::Plugin(err) => err.is_terminal(),
            Self::Session(err) => matches!(
                err,
                SessionError::ProviderMismatch { .. }
                    | SessionError::ProviderUnconfigured { .. }
                    | SessionError::ProviderUnavailable { .. }
                    | SessionError::CodeExecutionUnavailable
                    | SessionError::Store {
                        source: lash_core::StoreError::SessionDeleted { .. }
                            | lash_core::StoreError::CommitNodeBudgetExceeded { .. }
                            | lash_core::StoreError::CommitByteBudgetExceeded { .. }
                            | lash_core::StoreError::CheckpointComponentEncodingVersionMismatch { .. }
                            | lash_core::StoreError::RecordEncodingFailed { .. },
                        ..
                    }
            ),
            _ => false,
        }
    }
}

/// Result type returned by Lash facade operations.
pub type Result<T> = std::result::Result<T, EmbedError>;

#[cfg(test)]
mod tests {
    use super::EmbedError;
    use crate::runtime::{QueuedWorkRunError, QueuedWorkRunErrorClass};
    use lash_core::{
        PluginError, RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause,
        RuntimeErrorCode, SessionError, StoreError,
    };

    fn runtime_error(code: RuntimeErrorCode) -> EmbedError {
        EmbedError::Runtime(RuntimeError::new(code, "test"))
    }

    #[test]
    fn managed_turn_cap_denial_stays_retryable_across_plugin_host_boundaries() {
        let plugin_error = PluginError::Runtime(RuntimeError::new(
            RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded,
            "test cap reached",
        ));
        let embed_error = EmbedError::from(plugin_error.clone());
        assert!(embed_error.is_retryable());
        assert!(!embed_error.is_terminal());

        let queued_error = QueuedWorkRunError::from(plugin_error);
        assert_eq!(queued_error.class, QueuedWorkRunErrorClass::Transient);
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
        let err = runtime_error(RuntimeErrorCode::StoreCommitContended);
        assert!(err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
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

    #[test]
    fn untyped_runtime_failures_are_neither_retryable_nor_terminal() {
        let err = runtime_error(RuntimeErrorCode::from_wire_code("plugin_defined_abort"));
        assert!(!err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
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
            EmbedError::MissingEffectHost,
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

        for error in runtime_errors.into_iter().chain(session_errors) {
            assert!(error.is_terminal(), "{error}");
            assert!(!error.is_retryable(), "{error}");
        }
    }

    #[test]
    fn deleted_sessions_are_terminal_in_direct_and_wrapped_store_shapes() {
        let direct = EmbedError::Store(StoreError::SessionDeleted {
            session_id: "retired-direct".to_string(),
        });
        let wrapped = EmbedError::Session(SessionError::Store {
            context: "failed to bind retired session".to_string(),
            source: StoreError::SessionDeleted {
                session_id: "retired-wrapped".to_string(),
            },
        });
        let controller_owned = EmbedError::Runtime(
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "retired controller-owned session",
            )
            .with_cause(RuntimeErrorCause::SessionDeleted {
                session_id: "retired-controller-owned".to_string(),
            }),
        );
        let nested_controller_owned = EmbedError::Plugin(PluginError::RuntimeEffectController(
            RuntimeEffectControllerError::from(StoreError::SessionDeleted {
                session_id: "retired-nested-controller-owned".to_string(),
            }),
        ));

        for error in [direct, wrapped, controller_owned, nested_controller_owned] {
            assert!(error.is_terminal(), "{error}");
            assert!(!error.is_retryable(), "{error}");
        }
    }
}
