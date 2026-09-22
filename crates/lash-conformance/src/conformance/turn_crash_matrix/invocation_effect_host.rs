//! The journaled-tier effect host for the cold-process crash matrix: a thin
//! projection over the store's effect controller, which carries every
//! capability the host lends (await events, group executors, bound group-child
//! scopes) on the shared replay driver.

use std::sync::Arc;

use lash_sansio::SessionId;

use crate::RuntimeEffectController;

pub(super) struct InvocationEffectHost {
    pub(super) inner: Arc<dyn RuntimeEffectController>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for InvocationEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::EffectHost for InvocationEffectHost {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    fn turn_control_binding_id(&self) -> String {
        self.inner
            .await_event_authority_binding_id()
            .expect("invocation authority")
    }
    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

    /// The host is a thin projection over `inner` for the journaled tier, so
    /// tool-child wiring registers the resolver on the same replay driver the
    /// controller's group operations read — not on this wrapper, which owns no
    /// table.
    fn install_tool_child_host(
        &self,
        candidate: Arc<crate::runtime::effect::ToolChildHost>,
    ) -> Option<Arc<crate::runtime::effect::ToolChildHost>> {
        self.inner
            .register_group_executors(
                Arc::clone(&candidate) as Arc<dyn crate::runtime::effect::GroupExecutors>
            )
            .ok()?;
        Some(candidate)
    }

    /// This wrapper mints no bound controller itself — the journaled substrate
    /// does, bound to `binding` exactly as a store host's
    /// `scoped_for_group_child` would (FIG-3470).
    fn scoped_for_group_child(
        &self,
        admitted: crate::AdmittedScope,
        binding: crate::GroupChildBinding,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        self.inner.group_child_scoped_controller(admitted, binding)
    }
    fn scoped<'run>(
        &'run self,
        scope: crate::AdmittedScope,
    ) -> Result<crate::ScopedEffectController<'run>, crate::RuntimeError> {
        crate::ScopedEffectController::shared(Arc::clone(&self.inner), scope)
    }

    /// The turn driver registers its live opener only against a lendable
    /// controller; without this the recovered turn's tool children would name
    /// an opener no resolver on this host is running.
    fn scoped_static(
        &self,
        scope: crate::AdmittedScope,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        crate::ScopedEffectController::shared(Arc::clone(&self.inner), scope).map(Some)
    }
}
