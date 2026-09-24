use crate::runtime_support::effect_controller_doubles::*;
use crate::runtime_support::*;
use std::time::Instant;

#[async_trait::async_trait]
impl lash_core::EffectHost for RecordingEffectController {
    fn turn_control_binding_id(&self) -> String {
        self.await_event_authority_binding_id()
            .expect("recorder authority")
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::borrowed(self, scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        ScopedEffectController::shared(Arc::new(self.clone()), scope).map(Some)
    }

    // Group children minted under this host run through the recorder itself:
    // the substrate-facing group calls (`commit_group_child_final`,
    // `await_group_child_drain_admission`, the group driver) forward to the
    // embedded native controller where the opens landed.
    fn scoped_for_group_child(
        &self,
        scope: lash_core::AdmittedScope,
        _binding: lash_core::GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        ScopedEffectController::shared(Arc::new(self.clone()), scope).map(Some)
    }

    fn install_tool_child_host(
        &self,
        candidate: Arc<lash_core::facade_support::ToolChildHost>,
    ) -> Option<Arc<lash_core::facade_support::ToolChildHost>> {
        let installed = self.tool_children.get_or_init(|| candidate);
        self.native
            .register_group_executors(Arc::clone(installed) as Arc<dyn lash_core::GroupExecutors>)
            .ok()?;
        Some(Arc::clone(installed))
    }
}

/// Re-install the tool-child host after a test swaps `control.effect_host`:
/// the config's constructor registered the resolver on the host it was built
/// with, and a swapped-in host must register on its own controller — where
/// the double forwards group opens — or group children have no runner.
pub fn reinstall_tool_child_host(config: &mut RuntimeHostConfig) {
    let host = Arc::clone(&config.control.effect_host);
    config.control.tool_children =
        host.install_tool_child_host(lash_core::facade_support::ToolChildHost::new(
            &host,
            Arc::clone(&config.durability.process_env_store),
        ));
    if let Some(tool_children) = &config.control.tool_children {
        tool_children.with_clock(Arc::clone(&config.clock));
    }
}

pub fn host_with_effect_recorder(recorder: RecordingEffectController) -> EmbeddedRuntimeHost {
    let mut config = if recorder.controller_owned_replay {
        let mut config = test_runtime_host_config();
        config.control.effect_host = Arc::new(recorder);
        reinstall_tool_child_host(&mut config);
        config
    } else {
        runtime_host_config_with_native_controller(Arc::new(recorder))
    };
    config.providers.provider_resolver =
        Arc::new(lash_core::facade_support::SingleProviderResolver::new(
            mock_provider(Vec::new()).into_handle(),
        ));
    EmbeddedRuntimeHost::new(config)
}

pub fn runtime_host_config_with_native_controller(
    controller: Arc<dyn RuntimeEffectController>,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config();
    config.control.effect_host = controller_effect_host(controller);
    reinstall_tool_child_host(&mut config);
    config
}

struct ControllerEffectHost {
    controller: Arc<dyn RuntimeEffectController>,
    tool_children: std::sync::OnceLock<Arc<lash_core::facade_support::ToolChildHost>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for ControllerEffectHost {
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
impl lash_core::EffectHost for ControllerEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.controller
            .await_event_authority_binding_id()
            .expect("test controller must identify its authority")
    }
    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }
    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::borrowed(self.controller.as_ref(), scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        ScopedEffectController::shared(Arc::clone(&self.controller), scope).map(Some)
    }

    fn scoped_for_group_child(
        &self,
        scope: lash_core::AdmittedScope,
        _binding: lash_core::GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        ScopedEffectController::shared(Arc::clone(&self.controller), scope).map(Some)
    }

    fn install_tool_child_host(
        &self,
        candidate: Arc<lash_core::facade_support::ToolChildHost>,
    ) -> Option<Arc<lash_core::facade_support::ToolChildHost>> {
        let installed = self.tool_children.get_or_init(|| candidate);
        self.controller
            .register_group_executors(Arc::clone(installed) as Arc<dyn lash_core::GroupExecutors>)
            .ok()?;
        Some(Arc::clone(installed))
    }
}

pub fn controller_effect_host(
    controller: Arc<dyn RuntimeEffectController>,
) -> Arc<dyn lash_core::EffectHost> {
    assert!(
        controller.await_event_authority_binding_id().is_some(),
        "test controller must identify its authority"
    );
    Arc::new(ControllerEffectHost {
        controller,
        tool_children: std::sync::OnceLock::new(),
    })
}
