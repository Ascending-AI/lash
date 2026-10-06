use crate::runtime_support::effect_controller_doubles::*;
use crate::runtime_support::*;

/// `backend`'s effect host under `layer`: the host a test installs to record
/// or perturb the effect boundary of a runtime over `backend` (FIG-3580).
pub fn layered_effect_host(
    backend: &lash_core::Backend,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> Arc<dyn lash_core::EffectHost> {
    Arc::new(lash_core::testing::LayeredEffectHost::new(
        backend.effect_host(),
        layer,
    ))
}

/// `layer`'s scoped controller for `admitted` over `backend`'s effect host.
pub fn layered_scope(
    backend: &lash_core::Backend,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
    admitted: lash_core::AdmittedScope,
) -> ScopedEffectController<'static> {
    layered_effect_host(backend, layer)
        .scoped_static(admitted)
        .expect("admit the layered scope")
        .expect("the backend host lends a static controller")
}
