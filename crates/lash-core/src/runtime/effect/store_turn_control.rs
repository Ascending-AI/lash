use crate::{EffectHost, SessionId};
use std::sync::Arc;
use std::time::Instant;

/// Bind reserved Native cancellation promises to the supplied session store.
/// Every runtime entry path uses the same store-owned authority across reopen.
pub fn bind_store_turn_control_authority(
    owner: Arc<dyn EffectHost>,
    store: &dyn crate::RuntimePersistence,
) -> Result<Arc<dyn EffectHost>, crate::RuntimeError> {
    if owner.turn_control_authority_owner() != crate::TurnControlAuthorityOwner::SessionStore {
        return Ok(owner);
    }
    let authority = store.turn_cancellation_authority().ok_or_else(|| {
        crate::RuntimeError::new(
            crate::RuntimeErrorCode::InvalidTurnCancelRequest,
            "the configured effect host delegates turn cancellation to a session store that exposes no recoverable authority",
        )
    })?;
    let peek_controller = Arc::new(StoreDelegatedTurnControlPeekController {
        resolver: authority.resolver(),
    });
    Ok(Arc::new(StoreDelegatedTurnControlHost {
        owner,
        authority,
        peek_controller,
    }))
}

/// Routes only Lash's reserved turn-control promises to the session store.
/// All effect execution and ordinary await-event operations stay on the
/// configured Native host.
struct StoreDelegatedTurnControlHost {
    owner: Arc<dyn EffectHost>,
    authority: crate::TurnCancellationAuthority,
    peek_controller: Arc<StoreDelegatedTurnControlPeekController>,
}

struct StoreDelegatedTurnControlPeekController {
    resolver: Arc<dyn crate::AwaitEventResolver>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for StoreDelegatedTurnControlPeekController {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for StoreDelegatedTurnControlPeekController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::PeekAwaitEvent { key } = envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::TurnControlPeekOutcome,
                "the store-delegated turn-control peek controller received a non-peek effect",
            ));
        };
        let resolution = self
            .resolver
            .peek_await_event(&key)
            .await
            .map_err(crate::RuntimeEffectControllerError::from)?;
        Ok(crate::RuntimeEffectOutcome::PeekAwaitEvent { resolution })
    }
}

impl StoreDelegatedTurnControlHost {
    fn resolver_for_key(&self, key: &crate::AwaitEventKey) -> Arc<dyn crate::AwaitEventResolver> {
        if key.wait.is_turn_control() {
            self.authority.resolver()
        } else {
            Arc::clone(&self.owner) as Arc<dyn crate::AwaitEventResolver>
        }
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for StoreDelegatedTurnControlHost {
    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
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
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        if wait.is_turn_control() {
            self.authority.resolver().await_event_key(scope, wait).await
        } else {
            self.owner.await_event_key(scope, wait).await
        }
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.resolver_for_key(key)
            .resolve_await_event(key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.resolver_for_key(key).peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.resolver_for_key(key)
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.authority
            .resolver()
            .revoke_await_events_for_session(session_id)
            .await?;
        self.owner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
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
    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::AwaitEventKey>, crate::RuntimeError> {
        self.owner
            .list_outstanding_await_event_keys(session_id)
            .await
    }

    fn turn_attach(&self) -> Option<Arc<dyn crate::facade_support::TurnAttach>> {
        self.owner.turn_attach()
    }

    fn scoped<'run>(
        &'run self,
        scope: crate::ExecutionScope,
    ) -> Result<crate::ScopedEffectController<'run>, crate::RuntimeError> {
        self.owner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: crate::ExecutionScope,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        self.owner.scoped_static(scope)
    }

    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a crate::ScopedEffectController<'_>,
    ) -> Result<crate::TurnControlBinding<'a>, crate::RuntimeError> {
        if scoped.controller().turn_control_participation().await?
            == crate::TurnControlParticipation::DurableJournaled
        {
            return self.owner.turn_control_binding(scoped).await;
        }
        let binding_id = self.authority.binding_id().to_string();
        Ok(crate::TurnControlBinding::HostOwned {
            binding_id,
            resolver: self,
            peek: crate::ScopedEffectController::shared(
                Arc::clone(&self.peek_controller) as Arc<dyn crate::RuntimeEffectController>,
                scoped.execution_scope().clone(),
            )?,
            turn_attach: crate::TurnControlAttachment::Resolver(self),
        })
    }

    async fn retire_effect_journal(
        &self,
        retirement: crate::EffectJournalRetirement,
    ) -> Result<usize, crate::RuntimeError> {
        self.owner.retire_effect_journal(retirement).await
    }

    async fn reinstate_effect_scope(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<(), crate::RuntimeError> {
        self.owner.reinstate_effect_scope(scope).await
    }

    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        self.owner.effect_scope_fence_database()
    }

    fn bind_process_registry(&self, binding: crate::ProcessRegistryBinding) {
        self.owner.bind_process_registry(binding);
    }
}
