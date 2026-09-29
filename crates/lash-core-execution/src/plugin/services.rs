use std::sync::Arc;

use super::*;

#[derive(Debug, thiserror::Error)]
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
    #[error("plugin operation failed: {0}")]
    Failed(String),
    /// The store refused a write the operation made, with its typed answer
    /// kept whole: a `/compact` whose drive fence a newer admission
    /// superseded while it ran is refused with
    /// [`StoreError::StaleDriveFence`](crate::StoreError::StaleDriveFence)
    /// (FIG-4134).
    #[error("plugin operation's store write was refused: {0}")]
    Store(crate::StoreError),
}

#[derive(Clone)]
pub struct RuntimeServices {
    pub plugins: Arc<PluginSession>,
    pub tool_children: Option<Arc<crate::runtime::effect::ToolChildHost>>,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub clock: Arc<dyn crate::Clock>,
    /// The session's view of the store it persists through.
    pub store: Option<crate::store::SessionStore>,
    /// Manifest persistence may differ from runtime-state persistence for
    /// ephemeral process runtimes whose manifest rows belong to a parent
    /// session's store.
    pub attachment_manifest_store: Option<Arc<dyn crate::store::RuntimeStore>>,
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
    /// Services over the attachment facade and process-exec-env store the
    /// caller's backend supplies. There is no in-memory default (ADR 0102):
    /// a runtime with no session store still takes both ports from its host.
    pub fn new(
        plugins: Arc<PluginSession>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            plugins,
            tool_children: None,
            attachment_store,
            process_env_store,
            clock: Arc::new(crate::SystemClock),
            store: None,
            attachment_manifest_store: None,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_tool_children(
        mut self,
        tool_children: Option<Arc<crate::runtime::effect::ToolChildHost>>,
    ) -> Self {
        self.tool_children = tool_children;
        self
    }

    /// Rebind the attachment facade: a runtime binds its services to the
    /// session-scoped facade its host wraps around the backend's port.
    pub fn with_attachment_store(
        mut self,
        attachment_store: Arc<crate::SessionAttachmentStore>,
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
        attachment_store: Arc<crate::SessionAttachmentStore>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        Self(RuntimeServices {
            plugins,
            tool_children: None,
            attachment_store,
            process_env_store,
            clock: Arc::new(crate::SystemClock),
            attachment_manifest_store: Some(Arc::clone(store.store())),
            store: Some(store),
        })
    }

    pub fn with_attachment_manifest_store(
        mut self,
        store: Arc<dyn crate::store::RuntimeStore>,
    ) -> Self {
        self.0.attachment_manifest_store = Some(store);
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
