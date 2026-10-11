use lash::tools::ToolBinding;

fn rlm_tool_types_are_nameable(binding: ToolBinding) {
    let _ = binding;
}

fn rlm_core_type_is_nameable(core: lash::LashCore) {
    let _ = core;
}

fn main() {
    fn local_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
    local_serde::<lash::workflow::WorkflowUnavailable>();
    let _ = rlm_tool_types_are_nameable;
    let _ = rlm_core_type_is_nameable;
}

fn resolver_factory_uses_facade_types(
    factory: lash::rlm::RlmProtocolPluginFactory,
    resolver: lash::tools::SharedDeferredToolResolver,
) -> lash::rlm::RlmProtocolPluginFactory {
    factory.with_deferred_tool_resolver_factory(move |ctx: &lash::plugins::PluginSessionContext| {
        let _ = (&ctx.owner, &ctx.plugin_config);
        Ok::<_, lash::plugins::PluginError>(resolver.clone())
    })
}
