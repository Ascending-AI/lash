// FIG-4374: a host serves models through one registry and selects them by
// key. The builder takes no transport and no model metadata; a session and a
// send name a key, and nothing names a provider route.

fn core_builder_takes_no_transport(
    backend: lash::Backend,
    provider: lash::provider::ProviderHandle,
) {
    let _ = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider);
}

fn core_builder_selects_a_key_not_metadata(backend: lash::Backend, model: lash::ModelMetadata) {
    let _ = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded).model(model);
}

fn a_session_takes_no_transport(core: lash::LashCore, provider: lash::provider::ProviderHandle) {
    let _ = core.session("keyed").provider(provider);
}

fn a_send_names_no_provider_route(builder: lash::SendBuilder) {
    let _ = builder.provider_id("route");
}

fn main() {}
