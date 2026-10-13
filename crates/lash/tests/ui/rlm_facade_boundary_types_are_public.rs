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
    local_serde::<lash::tools::DeferredMatch>();
    local_serde::<lash::tools::ExecutionBinding>();
    let _ = deferred_resolution_uses_facade_types;
    let _ = rlm_tool_types_are_nameable;
    let _ = rlm_core_type_is_nameable;
}

fn deferred_resolution_uses_facade_types(
    cx: &lash::tools::DeferredResolveContext<'_>,
    provider: std::sync::Arc<dyn lash::tools::DeferredToolProvider>,
) {
    let _ = (&cx.owner, &cx.run, &cx.config, provider);
}
