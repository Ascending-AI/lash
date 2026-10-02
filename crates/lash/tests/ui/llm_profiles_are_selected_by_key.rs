// FIG-4374: a host serves models through one registry and selects them by
// key. The builder takes no transport; a session's spec and a send name a
// key, never model metadata, and nothing names a provider route.

fn core_builder_takes_no_transport(
    backend: lash::Backend,
    provider: lash::provider::ProviderHandle,
) {
    let _ = lash::LashCore::standard_builder(backend)
        .provider(provider);
}

fn a_spec_selects_a_key_not_metadata(model: lash::LlmProfileMetadata) {
    let _ = lash::SessionSpec::new(
        model,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    );
}

fn a_session_takes_no_transport(core: lash::LashCore, provider: lash::provider::ProviderHandle) {
    let _ = core.session("keyed").provider(provider);
}

fn a_send_names_no_provider_route(builder: lash::SendBuilder) {
    let _ = builder.provider_id("route");
}

fn main() {}
