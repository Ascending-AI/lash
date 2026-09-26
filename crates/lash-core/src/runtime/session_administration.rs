use std::sync::Arc;

use super::{
    AdmittedScope, EffectHost, ProcessWorkWiring, RuntimeError, ScopedEffectController,
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
    session_close: crate::drive::SessionCloseServices,
}

impl SessionAdministration {
    /// Construct the selected lifecycle administration services.
    ///
    /// This is a trusted host integration boundary. The host must compose all
    /// supplied services from the same physical persistence deployment.
    pub fn new(
        store_factory: Arc<dyn SessionStoreFactory>,
        effect_host: Arc<dyn EffectHost>,
        process: Option<ProcessWorkWiring>,
        trigger_store: Option<Arc<dyn crate::TriggerStore>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        process_engines: crate::ProcessEngineRegistry,
        session_close: crate::drive::SessionCloseServices,
    ) -> Self {
        Self {
            store_factory,
            effect_host,
            process,
            trigger_store,
            process_env_store,
            process_engines,
            session_close,
        }
    }

    /// Replace the execution host while retaining this administration owner.
    ///
    /// Backend adapters use this only while installing their own executor.
    pub fn with_effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = effect_host;
        self
    }

    pub fn store_factory(&self) -> &Arc<dyn SessionStoreFactory> {
        &self.store_factory
    }

    pub fn effect_host(&self) -> &Arc<dyn EffectHost> {
        &self.effect_host
    }

    pub fn process(&self) -> Option<&ProcessWorkWiring> {
        self.process.as_ref()
    }

    pub fn trigger_store(&self) -> Option<&Arc<dyn crate::TriggerStore>> {
        self.trigger_store.as_ref()
    }

    pub fn process_env_store(&self) -> &Arc<dyn crate::ProcessExecutionEnvStore> {
        &self.process_env_store
    }

    pub fn process_engines(&self) -> &crate::ProcessEngineRegistry {
        &self.process_engines
    }

    /// What a deletion's close runs against (FIG-3600 S7): the session work
    /// engine whose executions it releases, and the scope owner it closes the
    /// session's scopes with.
    pub fn session_close(&self) -> &crate::drive::SessionCloseServices {
        &self.session_close
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
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'a>, RuntimeError>;
}

impl SessionDeleteExecution for SessionAdministration {
    fn administration(&self) -> &SessionAdministration {
        self
    }

    fn scoped<'a>(
        &'a self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'a>, RuntimeError> {
        self.effect_host.scoped(admitted)
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
        let controller = executor.scoped(AdmittedScope::session_delete(&session_id))?;
        Ok(Self {
            session_id,
            administration: executor.administration().clone(),
            controller,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn administration(&self) -> &SessionAdministration {
        &self.administration
    }

    pub fn controller(&self) -> &ScopedEffectController<'a> {
        &self.controller
    }
}
