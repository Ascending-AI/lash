fn model_selection_requires_variant(backend: lash::Backend) {
    let _ = lash::LashCore::standard_builder(backend.clone(), lash::TurnBudget::Unbounded)
        .model("model-only");
    let _ = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .model_variant("low");
}

fn turn_builder_has_no_model_overlay(builder: lash::TurnBuilder) {
    let _ = builder.model("model-only");
}

fn main() {
    let _ = model_selection_requires_variant;
}
