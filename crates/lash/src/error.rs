use crate::support::*;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(
        "protocol plugin is required; call .protocol_plugin(...) or use LashCore::standard_builder()/LashCore::rlm_builder(...)"
    )]
    MissingProtocolPlugin,
    #[error("model spec is required; hosts must supply explicit model metadata")]
    MissingModelSpec,
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
    #[error("failed to create store for session `{session_id}`: {message}")]
    StoreFactory { session_id: String, message: String },
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
    ProcessExecutionConcurrency(#[from] lash_core::ProcessExecutionConcurrencyError),
    #[error("session deletion requires a LashCore store factory")]
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
    Control(#[from] lash_core::PluginOperationInvokeError),
}

impl EmbedError {
    /// True only when a typed signal says the failed operation is safe to
    /// retry as-is; `false` means "no typed retryable signal", not "known
    /// permanent" (see [`is_terminal`](Self::is_terminal) for that).
    ///
    /// The retryable set is enumerated deliberately from
    /// [`RuntimeErrorCode`](lash_core::RuntimeErrorCode):
    ///
    /// - [`SessionExecutionBusy`](lash_core::RuntimeErrorCode::SessionExecutionBusy):
    ///   another executor currently holds the session-execution lease; the
    ///   turn was rejected before any state changed, so retrying after a
    ///   backoff is safe.
    /// - [`SessionExecutionLeaseLost`](lash_core::RuntimeErrorCode::SessionExecutionLeaseLost):
    ///   the lease was fenced away mid-turn. The final commit is fenced on
    ///   the same lease, so the failed attempt committed nothing and its
    ///   queued-work/turn-input claims were released; a fresh attempt can
    ///   re-claim safely.
    /// - [`StoreCommitContended`](lash_core::RuntimeErrorCode::StoreCommitContended):
    ///   the backend aborted before publication because transactional write
    ///   authority was unavailable; retry the identical operation unchanged.
    ///
    /// Everything else is `false`. Notably
    /// [`StoreCommitFailed`](lash_core::RuntimeErrorCode::StoreCommitFailed)
    /// stays `false`: the code does not distinguish transient store I/O from
    /// conflicts, so there is no typed signal that a retry is safe.
    ///
    /// Provider failures never surface as `EmbedError` — a failed LLM call
    /// finishes the turn with `TurnOutcome::Stopped(ProviderError)` — so
    /// their typed retryability is carried on
    /// [`TurnIssue::retryable`](crate::turn::TurnIssue) instead.
    pub fn is_retryable(&self) -> bool {
        use lash_core::RuntimeErrorCode;
        match self {
            Self::Runtime(err) => matches!(
                err.code,
                RuntimeErrorCode::SessionExecutionBusy
                    | RuntimeErrorCode::SessionExecutionLeaseLost
                    | RuntimeErrorCode::StoreCommitContended
            ),
            _ => false,
        }
    }

    /// True only when a typed signal says retrying can never succeed without
    /// host-side changes (wiring, configuration, or invariant violations that
    /// a retry cannot repair). Errors that are neither
    /// [`is_retryable`](Self::is_retryable) nor terminal are simply unknown.
    ///
    /// The terminal set:
    ///
    /// - builder/wiring variants of this enum (missing protocol plugin,
    ///   model spec, effect host, stores, registries, handler context, and
    ///   store/session mismatches) — the same call fails identically until
    ///   the host changes its wiring;
    /// - [`RuntimeErrorCode`](lash_core::RuntimeErrorCode) wiring codes:
    ///   `MissingExecutionScopeId`, `ExecutionScopeTurnIdMismatch`,
    ///   `MissingProcessExecutionId`,
    ///   `DurableEffectLiveProtocolExtension`,
    ///   `DurableEffectLivePluginInput`;
    /// - session provider-configuration errors (`ProviderMismatch`,
    ///   `ProviderUnconfigured`, `ProviderUnavailable`,
    ///   `CodeExecutionUnavailable`).
    pub fn is_terminal(&self) -> bool {
        use lash_core::RuntimeErrorCode;
        match self {
            Self::MissingProtocolPlugin
            | Self::MissingModelSpec
            | Self::MissingEffectHost
            | Self::MissingAttachmentStore
            | Self::MissingProcessEnvStore
            | Self::StoreSessionMismatch { .. }
            | Self::MissingProcessWorkerStoreFactory
            | Self::ProcessRegistryRequiresStoreFactory
            | Self::MissingProcessRegistry
            | Self::ProcessExecutionConcurrency(_)
            | Self::MissingSessionStoreFactory
            | Self::MissingPluginTurnInput { .. }
            | Self::DurableEffectHostRequiresHandlerContext { .. }
            | Self::StaticTurnStreamRequiresStaticEffectHost => true,
            Self::Runtime(err) => matches!(
                err.code,
                RuntimeErrorCode::MissingExecutionScopeId
                    | RuntimeErrorCode::ExecutionScopeTurnIdMismatch
                    | RuntimeErrorCode::MissingProcessExecutionId
                    | RuntimeErrorCode::DurableEffectLiveProtocolExtension
                    | RuntimeErrorCode::DurableEffectLivePluginInput
            ),
            Self::Session(err) => matches!(
                err,
                SessionError::ProviderMismatch { .. }
                    | SessionError::ProviderUnconfigured { .. }
                    | SessionError::ProviderUnavailable { .. }
                    | SessionError::CodeExecutionUnavailable
            ),
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, EmbedError>;

#[cfg(test)]
mod tests {
    use super::EmbedError;
    use lash_core::{RuntimeError, RuntimeErrorCode};

    fn runtime_error(code: RuntimeErrorCode) -> EmbedError {
        EmbedError::Runtime(RuntimeError::new(code, "test"))
    }

    #[test]
    fn lease_contention_codes_are_retryable_and_not_terminal() {
        for code in [
            RuntimeErrorCode::SessionExecutionBusy,
            RuntimeErrorCode::SessionExecutionLeaseLost,
            RuntimeErrorCode::StoreCommitContended,
        ] {
            let err = runtime_error(code);
            assert!(err.is_retryable(), "{err}");
            assert!(!err.is_terminal(), "{err}");
        }
    }

    #[test]
    fn untyped_failures_are_neither_retryable_nor_terminal() {
        for err in [
            runtime_error(RuntimeErrorCode::StoreCommitFailed),
            runtime_error(RuntimeErrorCode::Other("plugin_defined_abort".into())),
        ] {
            assert!(!err.is_retryable(), "{err}");
            assert!(!err.is_terminal(), "{err}");
        }
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
}
