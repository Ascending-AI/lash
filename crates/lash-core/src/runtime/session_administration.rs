use crate::ActorContext;
use std::sync::Arc;

use super::AdmittedScope;
use super::ProcessWorkWiring;
use super::RuntimeError;
use lash_execution_contracts::deployment::DeploymentStore;
use lash_kernel_value::identity::SessionId;

/// Lifecycle services selected together for session administration.
///
/// This handle deliberately excludes provider, plugin, prompt, tracing, and
/// other live runtime policy. Backend integrations retain one handle and mint
/// operation contexts from the effect executor paired with that deployment.
#[derive(Clone)]
pub struct SessionAdministration {
    store_factory: Arc<dyn DeploymentStore>,
    effect_host: ActorContext,
    process: Option<ProcessWorkWiring>,
    process_env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
    process_engines: crate::ProcessEngineRegistry,
}

impl SessionAdministration {
    /// Construct the selected lifecycle administration services.
    ///
    /// This is a trusted host integration boundary. The host must compose all
    /// supplied services from the same physical persistence deployment.
    pub(crate) fn new(
        store_factory: Arc<dyn DeploymentStore>,
        effect_host: ActorContext,
        process: Option<ProcessWorkWiring>,
        process_env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
        process_engines: crate::ProcessEngineRegistry,
    ) -> Self {
        Self {
            store_factory,
            effect_host,
            process,
            process_env_store,
            process_engines,
        }
    }

    pub fn store_factory(&self) -> &Arc<dyn DeploymentStore> {
        &self.store_factory
    }

    pub fn process(&self) -> Option<&ProcessWorkWiring> {
        self.process.as_ref()
    }

    pub fn process_env_store(
        &self,
    ) -> &Arc<dyn lash_execution_contracts::process::ProcessEnvStore> {
        &self.process_env_store
    }

    pub fn process_engines(&self) -> &crate::ProcessEngineRegistry {
        &self.process_engines
    }

    /// Mint a delete context through this administration's retained host.
    pub fn delete_context(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<SessionDeleteContext<'_>, RuntimeError> {
        SessionDeleteContext::from_execution(self, session_id)
    }
}

/// Owner-issued capability for deleting exactly one session.
pub struct SessionDeleteContext<'a> {
    session_id: SessionId,
    administration: SessionAdministration,
    controller: ActorContext,
    /// The lifetime this value is bound to; the context it carries is `'static`.
    pub(crate) run: std::marker::PhantomData<&'a ()>,
}

impl<'a> SessionDeleteContext<'a> {
    fn from_execution(
        administration: &SessionAdministration,
        session_id: impl AsRef<str>,
    ) -> Result<Self, RuntimeError> {
        let session_id = SessionId::parse(session_id.as_ref())?;
        let controller = administration
            .effect_host
            .scoped(AdmittedScope::session_delete(&session_id))?;
        Ok(Self {
            run: std::marker::PhantomData,
            session_id,
            administration: administration.clone(),
            controller,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn administration(&self) -> &SessionAdministration {
        &self.administration
    }

    pub(crate) fn controller(&self) -> &ActorContext {
        &self.controller
    }
}
