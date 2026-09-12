use std::sync::Arc;

use super::{
    EffectHost, ExecutionScope, ProcessWorkWiring, RuntimeError, ScopedEffectController,
    SessionStoreFactory,
};
use crate::SessionId;

/// Lifecycle services selected together for session administration.
///
/// This handle deliberately excludes provider, plugin, prompt, tracing, and
/// other live runtime policy. Backend integrations retain one handle and mint
/// operation contexts from the effect executor paired with that deployment.
#[derive(Clone)]
pub struct SessionAdministration {
    store_factory: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn EffectHost>,
    process: Option<ProcessWorkWiring>,
    trigger_store: Option<Arc<dyn crate::TriggerStore>>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    process_engines: crate::ProcessEngineRegistry,
}

impl SessionAdministration {
    /// Construct the selected lifecycle administration services.
    ///
    /// This is a trusted host integration boundary. The host must compose all
    /// supplied services from the same physical persistence deployment.
    #[doc(hidden)]
    pub fn new(
        store_factory: Arc<dyn SessionStoreFactory>,
        effect_host: Arc<dyn EffectHost>,
        process: Option<ProcessWorkWiring>,
        trigger_store: Option<Arc<dyn crate::TriggerStore>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        process_engines: crate::ProcessEngineRegistry,
    ) -> Self {
        Self {
            store_factory,
            effect_host,
            process,
            trigger_store,
            process_env_store,
            process_engines,
        }
    }

    /// Replace the execution host while retaining this administration owner.
    ///
    /// Backend adapters use this only while installing their own executor.
    #[doc(hidden)]
    pub fn with_effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = effect_host;
        self
    }

    #[doc(hidden)]
    pub fn store_factory(&self) -> &Arc<dyn SessionStoreFactory> {
        &self.store_factory
    }

    #[doc(hidden)]
    pub fn effect_host(&self) -> &Arc<dyn EffectHost> {
        &self.effect_host
    }

    #[doc(hidden)]
    pub fn process(&self) -> Option<&ProcessWorkWiring> {
        self.process.as_ref()
    }

    #[doc(hidden)]
    pub fn trigger_store(&self) -> Option<&Arc<dyn crate::TriggerStore>> {
        self.trigger_store.as_ref()
    }

    #[doc(hidden)]
    pub fn process_env_store(&self) -> &Arc<dyn crate::ProcessExecutionEnvStore> {
        &self.process_env_store
    }

    #[doc(hidden)]
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

/// Trusted backend seam for issuing a deletion controller with its owner.
///
/// Rust keeps the returned controller and administration inseparable after
/// issuance. Implementations remain responsible for truthfully pairing their
/// SDK handler context with the physical deployment behind `administration`;
/// the type system cannot inspect external routing configuration.
pub trait SessionDeleteExecution {
    fn administration(&self) -> &SessionAdministration;

    fn scoped<'a>(
        &'a self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'a>, RuntimeError>;
}

impl SessionDeleteExecution for SessionAdministration {
    fn administration(&self) -> &SessionAdministration {
        self
    }

    fn scoped<'a>(
        &'a self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'a>, RuntimeError> {
        self.effect_host.scoped(scope)
    }
}

/// Owner-issued capability for deleting exactly one session.
pub struct SessionDeleteContext<'a> {
    session_id: SessionId,
    administration: SessionAdministration,
    controller: ScopedEffectController<'a>,
}

impl<'a> SessionDeleteContext<'a> {
    /// Mint a context from a trusted backend execution adapter.
    ///
    /// The session-delete scope is derived here; callers cannot supply a scope
    /// for one session alongside administration services for another.
    pub fn from_execution<E>(
        executor: &'a E,
        session_id: impl AsRef<str>,
    ) -> Result<Self, RuntimeError>
    where
        E: SessionDeleteExecution + ?Sized,
    {
        let session_id = SessionId::from(session_id.as_ref());
        let scope = ExecutionScope::session_delete(&session_id);
        let controller = executor.scoped(scope)?;
        Ok(Self {
            session_id,
            administration: executor.administration().clone(),
            controller,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[doc(hidden)]
    pub fn administration(&self) -> &SessionAdministration {
        &self.administration
    }

    #[doc(hidden)]
    pub fn controller(&self) -> &ScopedEffectController<'a> {
        &self.controller
    }
}
