use lash::tools::ToolBinding;

fn rlm_tool_types_are_nameable(binding: ToolBinding) {
    let _ = binding;
}

fn rlm_core_type_is_nameable(core: lash::LashCore) {
    let _ = core;
}

fn deferred_trigger_resolver_is_configurable(
    factory: lash::rlm::RlmProtocolPluginFactory,
    resolver: lash::rlm::SharedDeferredTriggerResolver,
) -> lash::rlm::RlmProtocolPluginFactory {
    factory.with_deferred_trigger_resolver(resolver)
}

fn deferred_trigger_provider_registry_is_constructible() {
    let _ = lash::rlm::DeferredTriggerProviderRegistry::new();
}

fn main() {
    let _ = rlm_tool_types_are_nameable;
    let _ = rlm_core_type_is_nameable;
    let _ = deferred_trigger_resolver_is_configurable;
    let _ = deferred_trigger_provider_registry_is_constructible;
}
