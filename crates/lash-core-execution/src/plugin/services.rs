use std::sync::Arc;

use super::*;

#[derive(Clone, Debug, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PluginOperationInvokeError {
    #[error("unknown plugin operation `{0}`")]
    Unknown(String),
    #[error("unknown plugin session `{0}`")]
    UnknownSession(String),
    #[error("plugin operation `{0}` requires a session")]
    MissingSession(String),
    #[error("plugin operation `{0}` does not accept a session")]
    UnexpectedSession(String),
    /// No plugin view of `session_id` is published on this process: a
    /// query reads the view a run or command published here, and none has
    /// (FIG-5139). Publish one with a session command, e.g. a tool-catalog
    /// refresh, then retry the query.
    #[error("no plugin view of session `{session_id}` is published on this process")]
    NotPublished { session_id: crate::SessionId },
    #[error("plugin operation failed: {0}")]
    Failed(Box<PluginOperationFailure>),
    #[error(transparent)]
    Runtime(Box<crate::RuntimeError>),
    #[error("plugin input admission refused: {0}")]
    AdmissionRefused(Box<crate::RuntimeError>),
}

impl PluginOperationInvokeError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Failed(failure) => failure.class == PluginFailureClass::Retryable,
            Self::Runtime(error) | Self::AdmissionRefused(error) => error.is_retryable(),
            _ => false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Failed(failure) => failure.class == PluginFailureClass::Terminal,
            Self::Runtime(error) | Self::AdmissionRefused(error) => error.is_terminal(),
            _ => true,
        }
    }

    pub fn into_runtime_error(self) -> crate::RuntimeError {
        match self {
            Self::Runtime(error) | Self::AdmissionRefused(error) => *error,
            error => PluginError::Operation(Box::new(error.into_failure()))
                .into_turn_failure(crate::RuntimeErrorCode::Plugin),
        }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Failed(Box::new(super::operation_protocol_failure(message)))
    }

    pub fn into_failure(self) -> PluginOperationFailure {
        match self {
            Self::Failed(failure) => *failure,
            Self::Runtime(error) | Self::AdmissionRefused(error) => {
                super::error::runtime_operation_failure(*error)
            }
            error => {
                let message = error.to_string();
                match serde_json::to_value(&error) {
                    Ok(payload) => PluginOperationFailure {
                        error_type: "lash.operation.invoke".into(),
                        error_version: std::num::NonZeroU32::MIN,
                        payload,
                        class: PluginFailureClass::Terminal,
                        code: crate::FailureCode::from(&crate::RuntimeErrorCode::Plugin),
                        message,
                        origin: None,
                    },
                    Err(error) => super::operation_protocol_failure(error.to_string()),
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct RuntimeServices {
    pub plugins: Arc<PluginSession>,

    pub attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub clock: Arc<dyn crate::Clock>,
    /// The session's view of the store it persists through.
    pub store: Option<crate::store::SessionStore>,
    /// Manifest persistence may differ from runtime-state persistence for
    /// ephemeral process runtimes whose manifest rows belong to a parent
    /// session's store.
    pub attachment_referrers_store: Option<Arc<dyn crate::store::RuntimeStore>>,
}

#[derive(Clone)]
pub struct PersistentRuntimeServices(RuntimeServices);

impl std::ops::Deref for PersistentRuntimeServices {
    type Target = RuntimeServices;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
pub(crate) struct NoopSessionManager;

#[cfg(test)]
impl SessionReadService for NoopSessionManager {}
#[cfg(test)]
impl ProcessReadService for NoopSessionManager {}
#[cfg(test)]
impl SessionStateService for NoopSessionManager {}
#[cfg(test)]
impl SessionLifecycleService for NoopSessionManager {}
#[cfg(test)]
impl SessionGraphService for NoopSessionManager {}
impl RuntimeServices {
    /// Services over the attachment facade and process-exec-env store the
    /// caller's backend supplies. There is no in-memory default (ADR 0102):
    /// a runtime with no session store still takes both ports from its host.
    pub fn new(
        plugins: Arc<PluginSession>,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            plugins,

            attachment_store,
            process_env_store,
            clock: Arc::new(crate::SystemClock),
            store: None,
            attachment_referrers_store: None,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Rebind the attachment facade: a runtime binds its services to the
    /// session-scoped facade its host wraps around the backend's port.
    pub fn with_attachment_store(
        mut self,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
    ) -> Self {
        self.attachment_store = attachment_store;
        self
    }

    pub fn with_process_env_store(
        mut self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = process_env_store;
        self
    }
}

impl PersistentRuntimeServices {
    /// Services persisting through `store`, over the attachment facade and
    /// process-exec-env store the caller's backend supplies.
    pub fn new(
        plugins: Arc<PluginSession>,
        store: crate::store::SessionStore,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        Self(RuntimeServices {
            plugins,

            attachment_store,
            process_env_store,
            clock: Arc::new(crate::SystemClock),
            attachment_referrers_store: Some(Arc::clone(store.store())),
            store: Some(store),
        })
    }

    pub fn with_attachment_referrers_store(
        mut self,
        store: Arc<dyn crate::store::RuntimeStore>,
    ) -> Self {
        self.0.attachment_referrers_store = Some(store);
        self
    }

    pub fn into_runtime_services(self) -> RuntimeServices {
        self.0
    }

    #[expect(
        clippy::expect_used,
        reason = "the persistent constructor is the only one that hands out these services, and it always sets a store"
    )]
    pub fn store(&self) -> crate::store::SessionStore {
        self.0
            .store
            .as_ref()
            .expect("persistent runtime services must carry a store")
            .clone()
    }
}
