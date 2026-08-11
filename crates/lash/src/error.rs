use crate::support::*;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(
        "protocol plugin is required; call .protocol_plugin(...) or use LashCore::standard_builder(lash::TurnBudget::bounded(...))/LashCore::rlm_builder(lash::TurnBudget::bounded(...), ...)"
    )]
    MissingProtocolPlugin,
    #[error("model spec is required; hosts must supply explicit model metadata")]
    MissingModelSpec,
    #[error(
        "turn budget is required; SessionSpec must carry TurnBudget::Bounded(...) or TurnBudget::Unbounded"
    )]
    MissingTurnBudget,
    #[error("effect host is required; provide an explicit effect host with .effect_host(...)")]
    MissingEffectHost,
    #[error(
        "attachment store is required; provide an explicit attachment store with .attachment_store(...)"
    )]
    MissingAttachmentStore,
    #[error(
        "process execution environment store is required; provide an explicit process env store with .process_env_store(...)"
    )]
    MissingProcessEnvStore,
    #[error(
        "commit budget is required; provide explicit byte and node limits with .commit_budget(...)"
    )]
    MissingCommitBudget,
    #[error(
        "queued-work batching policy is required; provide an explicit model-action reserve with .queued_work_batching(...)"
    )]
    MissingQueuedWorkBatching,
    #[error("failed to create store for session `{session_id}`: {message}")]
    StoreFactory { session_id: String, message: String },
    #[error("session store operation failed: {0}")]
    Store(#[from] lash_core::StoreError),
    #[error(
        "store-less session id `{session_id}` was already used by this LashCore; store-less sessions require distinct ids per process"
    )]
    EphemeralSessionIdReused { session_id: String },
    #[error("store is bound to session `{loaded}` but builder requested `{requested}`")]
    StoreSessionMismatch { loaded: String, requested: String },
    #[error("durable process worker requires a LashCore store factory")]
    MissingProcessWorkerStoreFactory,
    #[error(
        "a process registry is configured for the default inline process work runner but no session store factory is wired; the runner rebuilds a session runtime per process and cannot do so without one. Wire .store_factory(...) - InMemorySessionStoreFactory::new() for ephemeral process execution, or a durable factory - or use .process_work_driver(...) for an externally driven durable runner."
    )]
    ProcessRegistryRequiresStoreFactory,
    #[error("durable process worker config requires a LashCore process registry")]
    MissingProcessRegistry,
    #[error("invalid process execution configuration: {0}")]
    ProcessExecutionConcurrency(
        #[from] lash_core::facade_support::ProcessExecutionConcurrencyError,
    ),
    #[error("invalid queued-work execution configuration: {0}")]
    QueuedWorkExecutionConcurrency(
        #[from] lash_core::facade_support::QueuedWorkExecutionConcurrencyError,
    ),
    #[error("this operation requires a LashCore store factory")]
    MissingSessionStoreFactory,
    #[error("failed to delete process state for session `{session_id}`: {message}")]
    SessionDeleteProcess { session_id: String, message: String },
    #[error("missing required turn input for plugin `{plugin_id}`")]
    MissingPluginTurnInput { plugin_id: &'static str },
    #[error(
        "session is still in use: park()/close() consume the session and require exclusive ownership; drop any cloned handles and finish or cancel in-flight turns first"
    )]
    SessionStillInUse,
    #[error("failed to flush trace sink: {0}")]
    TraceFlush(#[from] lash_trace::TraceSinkError),
    #[error(
        "configured effect host for {operation} is durable and requires a handler context; use .effects(&controller) and provide .turn_id(...) for replayable foreground requests"
    )]
    DurableEffectHostRequiresHandlerContext { operation: &'static str },
    #[error(
        "pull-style turn streams require an effect host that can create a static scoped controller; use stream_to(...) inside the handler context"
    )]
    StaticTurnStreamRequiresStaticEffectHost,
    #[error("runtime session error: {0}")]
    Session(#[from] SessionError),
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
    /// True only when a typed signal says the failed operation is safe to
    /// retry as-is; `false` means "no typed retryable signal", not "known
    /// permanent" (see [`is_terminal`](Self::is_terminal) for that).
    ///
    /// Runtime failures delegate to the closed
    /// [`RuntimeErrorCode`](lash_core::RuntimeErrorCode) taxonomy. Its
    /// retryable set includes lease contention plus idempotent store, Restate
    /// ingress, session-refresh, and bounded-wait operations. Foreign extension
    /// codes are conservatively not retryable.
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
            Self::Plugin(lash_core::PluginError::Runtime(err)) => err.is_retryable(),
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
    /// - direct, session-wrapped, or nested controller-owned
    ///   [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted)
    ///   tombstones — session ids are permanently single-use;
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
            | Self::StoreSessionMismatch { .. }
            | Self::MissingProcessWorkerStoreFactory
            | Self::ProcessRegistryRequiresStoreFactory
            | Self::MissingProcessRegistry
            | Self::ProcessExecutionConcurrency(_)
            | Self::QueuedWorkExecutionConcurrency(_)
            | Self::MissingSessionStoreFactory
            | Self::MissingPluginTurnInput { .. }
            | Self::DurableEffectHostRequiresHandlerContext { .. }
            | Self::StaticTurnStreamRequiresStaticEffectHost => true,
            Self::Store(
                lash_core::StoreError::SessionDeleted { .. }
                | lash_core::StoreError::CommitNodeBudgetExceeded { .. }
                | lash_core::StoreError::CommitByteBudgetExceeded { .. },
            ) => true,
            Self::Runtime(err) => err.is_terminal(),
            Self::Plugin(lash_core::PluginError::Runtime(err)) => err.is_terminal(),
            Self::Plugin(lash_core::PluginError::RuntimeEffectController(err)) => matches!(
                err.cause.as_ref(),
                Some(lash_core::RuntimeErrorCause::SessionDeleted { .. })
            ),
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
    fn session_execution_busy_is_retryable_and_not_terminal() {
        let err = runtime_error(RuntimeErrorCode::SessionExecutionBusy);
        assert!(err.is_retryable(), "{err}");
        assert!(!err.is_terminal(), "{err}");
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
                graph_delta_bytes: 2,
                checkpoint_bytes: 3,
                attachment_manifest_bytes: 5,
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
