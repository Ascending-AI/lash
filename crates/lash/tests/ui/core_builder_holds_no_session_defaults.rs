// FIG-4594: a core keeps no session defaults. Its builder takes deployment
// facts only (the backend, plugins, the model registry, tracing), so it has
// no turn budget argument and no model, reasoning, generation, attachment
// acceptance, plugin option or spec setter. A host that wants a default keeps
// its own `SessionSpec` value and passes it to each creation.

fn the_builder_takes_no_turn_budget(backend: lash::Backend) {
    let _ = lash::LashCore::builder(backend, lash::TurnBudget::Unbounded);
}

fn the_builder_selects_no_model(backend: lash::Backend, key: lash::LlmProfileKey) {
    let _ = lash::LashCore::standard_builder(backend).model(key);
}

fn the_builder_states_no_reasoning(
    backend: lash::Backend,
    reasoning: lash::provider::ReasoningSelection,
) {
    let _ = lash::LashCore::standard_builder(backend).reasoning(reasoning);
}

fn the_builder_states_no_generation(
    backend: lash::Backend,
    generation: lash::direct::GenerationOptions,
) {
    let _ = lash::LashCore::standard_builder(backend).generation(generation);
}

fn the_builder_states_no_attachment_acceptance(
    backend: lash::Backend,
    acceptance: std::sync::Arc<lash::provider::AttachmentCapabilitySnapshot>,
) {
    let _ = lash::LashCore::standard_builder(backend).attachment_acceptance(acceptance);
}

fn the_builder_states_no_plugin_options(backend: lash::Backend) {
    let _ = lash::LashCore::standard_builder(backend).session_plugin("plugin", ());
}

fn the_builder_holds_no_default_spec(backend: lash::Backend, spec: lash::SessionSpec) {
    let _ = lash::LashCore::standard_builder(backend).session_spec(spec);
}

fn main() {}
