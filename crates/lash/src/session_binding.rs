use crate::support::{
    Arc, EffectHost, ProcessWorkWiring, QueuedWorkSubstrate, RuntimeEnvironment,
    RuntimePersistence, SessionStoreFactory,
};
use lash_sansio::SessionId;
use std::time::Instant;

/// Routes only Lash's reserved turn-control promises to the session store.
/// All effect execution and ordinary await-event operations stay on the
/// configured Native host.
struct StoreDelegatedTurnControlHost {
    owner: Arc<dyn EffectHost>,
    authority: lash_core::TurnCancellationAuthority,
    peek_controller: Arc<StoreDelegatedTurnControlPeekController>,
}

struct StoreDelegatedTurnControlPeekController {
    resolver: Arc<dyn lash_core::AwaitEventResolver>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for StoreDelegatedTurnControlPeekController {}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for StoreDelegatedTurnControlPeekController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        _local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        let lash_core::RuntimeEffectCommand::PeekAwaitEvent { key } = envelope.command else {
            return Err(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::TurnControlPeekOutcome,
                "the store-delegated turn-control peek controller received a non-peek effect",
            ));
        };
        let resolution = self
            .resolver
            .peek_await_event(&key)
            .await
            .map_err(lash_core::RuntimeEffectControllerError::from)?;
        Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent { resolution })
    }
}

impl StoreDelegatedTurnControlHost {
    fn resolver_for_key(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Arc<dyn lash_core::AwaitEventResolver> {
        if key.wait.is_turn_control() {
            self.authority.resolver()
        } else {
            Arc::clone(&self.owner) as Arc<dyn lash_core::AwaitEventResolver>
        }
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for StoreDelegatedTurnControlHost {
    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        if wait.is_turn_control() {
            self.authority
                .resolver()
                .prepare_completion_key(scope, wait, may_defer)
                .await
        } else {
            self.owner
                .prepare_completion_key(scope, wait, may_defer)
                .await
        }
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        if wait.is_turn_control() {
            self.authority.resolver().await_event_key(scope, wait).await
        } else {
            self.owner.await_event_key(scope, wait).await
        }
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.resolver_for_key(key)
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.resolver_for_key(key).peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.resolver_for_key(key)
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.authority
            .resolver()
            .revoke_await_events_for_session(session_id)
            .await?;
        self.owner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), lash_core::RuntimeError> {
        self.authority
            .resolver()
            .cancel_await_events_for_session(session_id)
            .await?;
        self.owner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for StoreDelegatedTurnControlHost {
    fn turn_control_binding_id(&self) -> String {
        self.authority.binding_id().to_string()
    }
    fn turn_attach(&self) -> Option<Arc<dyn lash_core::facade_support::TurnAttach>> {
        self.owner.turn_attach()
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.owner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.owner.scoped_static(scope)
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a lash_core::ScopedEffectController<'_>,
    ) -> Result<lash_core::TurnControlBinding<'a>, lash_core::RuntimeError> {
        let binding_id = lash_core::facade_support::turn_control_binding_id_for_scope(
            self.authority.binding_id(),
            scoped.execution_scope(),
        )?;
        Ok(lash_core::TurnControlBinding::HostOwned {
            binding_id,
            resolver: self,
            peek: lash_core::ScopedEffectController::shared(
                Arc::clone(&self.peek_controller) as Arc<dyn lash_core::RuntimeEffectController>,
                scoped.execution_scope().clone(),
            )?,
            turn_attach: lash_core::TurnControlAttachment::Resolver(self),
        })
    }

    async fn retire_effect_journal(
        &self,
        retirement: lash_core::EffectJournalRetirement,
    ) -> Result<usize, lash_core::RuntimeError> {
        self.owner.retire_effect_journal(retirement).await
    }

    async fn reinstate_effect_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.owner.reinstate_effect_scope(scope).await
    }

    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        self.owner.effect_scope_fence_database()
    }

    fn bind_process_registry(&self, binding: lash_core::ProcessRegistryBinding) {
        self.owner.bind_process_registry(binding);
    }
}

/// Immutable owner-issued capabilities for one successfully opened session.
///
/// Construction stays inside the facade open/materialize paths. The store,
/// effect host, process/queue ports, trigger store, and optional catalog are
/// captured together so later per-session operations cannot independently
/// consult a core override. Backend adapters remain responsible for supplying
/// a truthful deployment composition when they wire these capabilities.
#[derive(Clone)]
pub(crate) struct BoundSession {
    session_id: SessionId,
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn EffectHost>,
    process: Option<ProcessWorkWiring>,
    queued: Arc<dyn QueuedWorkSubstrate>,
    trigger_store: Option<Arc<dyn lash_core::TriggerStore>>,
    child_store_provider: Option<Arc<dyn SessionStoreFactory>>,
    attachment_store: Arc<lash_core::facade_support::SessionAttachmentStore>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    catalog: Option<Arc<dyn SessionStoreFactory>>,
}

impl BoundSession {
    pub(crate) fn new(
        session_id: SessionId,
        store: Arc<dyn RuntimePersistence>,
        env: &RuntimeEnvironment,
        process: Option<ProcessWorkWiring>,
        queued: Arc<dyn QueuedWorkSubstrate>,
        catalog: Option<Arc<dyn SessionStoreFactory>>,
    ) -> Result<Self, lash_core::RuntimeError> {
        let configured_effect_host = Arc::clone(&env.core.control.effect_host);
        let effect_host = if configured_effect_host.turn_control_authority_owner()
            == lash_core::TurnControlAuthorityOwner::SessionStore
        {
            let authority = store.turn_cancellation_authority().ok_or_else(|| {
                lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::InvalidTurnCancelRequest,
                    "the configured effect host delegates turn cancellation to a session store that exposes no recoverable authority",
                )
            })?;
            let peek_controller = Arc::new(StoreDelegatedTurnControlPeekController {
                resolver: authority.resolver(),
            });
            Arc::new(StoreDelegatedTurnControlHost {
                owner: Arc::clone(&configured_effect_host),
                authority,
                peek_controller,
            }) as Arc<dyn EffectHost>
        } else {
            configured_effect_host
        };
        Ok(Self {
            session_id,
            store,
            effect_host,
            process,
            queued,
            trigger_store: env.trigger_store.clone(),
            child_store_provider: env.session_store_factory.clone(),
            attachment_store: Arc::clone(&env.core.durability.attachment_store),
            process_env_store: Arc::clone(&env.core.durability.process_env_store),
            catalog,
        })
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn store(&self) -> Arc<dyn RuntimePersistence> {
        Arc::clone(&self.store)
    }

    pub(crate) fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub(crate) fn process(&self) -> Option<&ProcessWorkWiring> {
        self.process.as_ref()
    }

    pub(crate) fn catalog(&self) -> Option<Arc<dyn SessionStoreFactory>> {
        self.catalog.clone()
    }

    pub(crate) fn administration(&self) -> Option<lash_core::SessionAdministration> {
        self.catalog().map(|catalog| {
            lash_core::SessionAdministration::new(
                catalog,
                self.effect_host(),
                self.process.clone(),
                self.trigger_store.clone(),
            )
        })
    }

    /// Apply only lifecycle-owner services to a destination core environment.
    /// Provider, plugin, prompt, tracing, and policy configuration continue to
    /// come from the core performing resume.
    pub(crate) fn apply_owner(&self, mut env: RuntimeEnvironment) -> RuntimeEnvironment {
        env.core.control.effect_host = self.effect_host();
        env.trigger_store = self.trigger_store.clone();
        env.session_store_factory = self.child_store_provider.clone();
        env.core.durability.attachment_store = Arc::clone(&self.attachment_store);
        env.core.durability.process_env_store = Arc::clone(&self.process_env_store);
        env.with_work_ports(self.process.clone(), Arc::clone(&self.queued))
    }
}
