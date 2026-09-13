use super::*;
use std::time::Instant;

#[async_trait::async_trait]
impl crate::EffectHost for RecordingEffectController {
    fn turn_control_binding_id(&self) -> String {
        self.await_event_authority_binding_id()
            .expect("recorder authority")
    }

    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::borrowed(self, scope)
    }
}

pub(in crate::runtime::tests) fn host_with_effect_recorder(
    recorder: RecordingEffectController,
) -> EmbeddedRuntimeHost {
    let mut config = if recorder.controller_owned_replay {
        let mut config = test_runtime_host_config();
        config.control.effect_host = Arc::new(recorder);
        config
    } else {
        runtime_host_config_with_native_controller(Arc::new(recorder))
    };
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
}

pub(in crate::runtime::tests) fn runtime_host_config_with_native_controller(
    controller: Arc<dyn RuntimeEffectController>,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config();
    config.control.effect_host = controller_effect_host(controller);
    config
}

struct ControllerEffectHost {
    controller: Arc<dyn RuntimeEffectController>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for ControllerEffectHost {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl crate::EffectHost for ControllerEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.controller
            .await_event_authority_binding_id()
            .expect("test controller must identify its authority")
    }
    fn await_event_resolver(&self) -> &dyn crate::AwaitEventResolver {
        self
    }
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::borrowed(self.controller.as_ref(), scope)
    }
}

pub(in crate::runtime::tests) fn controller_effect_host(
    controller: Arc<dyn RuntimeEffectController>,
) -> Arc<dyn crate::EffectHost> {
    assert!(
        controller.await_event_authority_binding_id().is_some(),
        "test controller must identify its authority"
    );
    Arc::new(ControllerEffectHost { controller })
}
