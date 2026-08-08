use std::sync::Arc;

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum PluginOperationInvokeError {
    #[error("unknown plugin operation `{0}`")]
    Unknown(String),
    #[error("unknown plugin session `{0}`")]
    UnknownSession(String),
    #[error("plugin operation `{0}` requires a session")]
    MissingSession(String),
    #[error("plugin operation `{0}` does not accept a session")]
    UnexpectedSession(String),
    #[error("plugin operation failed: {0}")]
    Failed(String),
}

#[derive(Clone)]
pub struct RuntimeServices {
    pub plugins: Arc<PluginSession>,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub clock: Arc<dyn crate::Clock>,
    pub(crate) store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    /// Manifest persistence may differ from runtime-state persistence for
    /// ephemeral process runtimes backed by a parent-bound session factory.
    pub(crate) attachment_manifest_store: Option<Arc<dyn crate::store::RuntimePersistence>>,
}

#[derive(Clone)]
pub struct PersistentRuntimeServices(RuntimeServices);

impl std::ops::Deref for PersistentRuntimeServices {
    type Target = RuntimeServices;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) struct NoopSessionManager;

#[cfg(any(test, feature = "testing"))]
impl SessionReadService for NoopSessionManager {}
#[cfg(any(test, feature = "testing"))]
impl ProcessReadService for NoopSessionManager {}
#[cfg(any(test, feature = "testing"))]
impl SessionStateService for NoopSessionManager {}
#[cfg(any(test, feature = "testing"))]
impl SessionLifecycleService for NoopSessionManager {}
#[cfg(any(test, feature = "testing"))]
impl SessionGraphService for NoopSessionManager {}
impl RuntimeServices {
    pub fn new(plugins: Arc<PluginSession>) -> Self {
        Self {
            plugins,
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            clock: Arc::new(crate::SystemClock),
            store: None,
            attachment_manifest_store: None,
        }
    }

    pub(crate) fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) fn with_attachment_store(
        mut self,
        attachment_store: Arc<crate::SessionAttachmentStore>,
    ) -> Self {
        self.attachment_store = attachment_store;
        self
    }

    pub(crate) fn with_process_env_store(
        mut self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = process_env_store;
        self
    }
}

impl PersistentRuntimeServices {
    pub fn new(
        plugins: Arc<PluginSession>,
        store: Arc<dyn crate::store::RuntimePersistence>,
    ) -> Self {
        Self(RuntimeServices {
            plugins,
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            clock: Arc::new(crate::SystemClock),
            store: Some(Arc::clone(&store)),
            attachment_manifest_store: Some(store),
        })
    }

    pub(crate) fn with_attachment_manifest_store(
        mut self,
        store: Arc<dyn crate::store::RuntimePersistence>,
    ) -> Self {
        self.0.attachment_manifest_store = Some(store);
        self
    }

    pub(crate) fn into_runtime_services(self) -> RuntimeServices {
        self.0
    }

    pub fn store(&self) -> Arc<dyn crate::store::RuntimePersistence> {
        self.0
            .store
            .as_ref()
            .expect("persistent runtime services must carry a store")
            .clone()
    }
}
