use crate::runtime_support::effect_controller_doubles::*;
use crate::runtime_support::*;

/// `backend`'s effect host under `layer`: the host a test installs to record
/// or perturb the effect boundary of a runtime over `backend` (FIG-3580).
pub fn layered_effect_host(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> Arc<dyn lash_core::EffectHost> {
    Arc::new(lash_core::testing::LayeredEffectHost::new(
        backend.effect_host(),
        layer,
    ))
}

/// `backend` with its effect host under `layer`.
pub fn backend_with_effect_layer(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> Arc<dyn lash_core::Backend> {
    lash_core::testing::runtime_helpers::LayeredBackend::over(Arc::clone(backend))
        .map_effect_host(|host| Arc::new(lash_core::testing::LayeredEffectHost::new(host, layer)))
        .into_backend()
}

/// A test host config over `backend` whose effect host is `backend`'s under
/// `layer`. The config is built over the layered backend rather than swapping
/// the host in afterwards: the first host config over a backend installs the
/// backend's one tool-child host, and a tool child must reach its effect
/// controller through the layer too.
pub fn runtime_host_config_with_effect_layer(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> RuntimeHostConfig {
    test_runtime_host_config(&backend_with_effect_layer(backend, layer))
}

/// `layer`'s scoped controller for `admitted` over `backend`'s effect host.
pub fn layered_scope(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
    admitted: lash_core::AdmittedScope,
) -> ScopedEffectController<'static> {
    layered_effect_host(backend, layer)
        .scoped_static(admitted)
        .expect("admit the layered scope")
        .expect("the backend host lends a static controller")
}

/// The recorder's scoped controller for `turn_id` of the root session.
pub fn scoped_test_turn(
    backend: &Arc<dyn lash_core::Backend>,
    recorder: &RecordingEffectController,
    turn_id: &TurnId,
) -> ScopedEffectController<'static> {
    layered_scope(
        backend,
        Arc::new(recorder.clone()),
        AdmittedScope::turn("root", turn_id),
    )
}

pub fn host_with_effect_recorder(
    backend: &Arc<dyn lash_core::Backend>,
    recorder: RecordingEffectController,
) -> EmbeddedRuntimeHost {
    let mut config = runtime_host_config_with_effect_layer(backend, Arc::new(recorder));
    config.providers.provider_resolver =
        Arc::new(lash_core::facade_support::SingleProviderResolver::new(
            mock_provider(Vec::new()).into_handle(),
        ));
    EmbeddedRuntimeHost::new(config)
}

/// `layer`'s controller for `admitted` over `backend`'s effect host, for a test
/// that drives the controller directly.
pub fn layered_controller(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
    admitted: lash_core::AdmittedScope,
) -> Arc<dyn RuntimeEffectController> {
    layered_scope(backend, layer, admitted)
        .owned_controller()
        .expect("a static layered controller is shared")
}

/// [`layered_controller`] for the runtime-operation scope a shared controller
/// handle admits.
pub fn layered_operation_controller(
    backend: &Arc<dyn lash_core::Backend>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> Arc<dyn RuntimeEffectController> {
    layered_controller(
        backend,
        layer,
        lash_core::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
    )
}
