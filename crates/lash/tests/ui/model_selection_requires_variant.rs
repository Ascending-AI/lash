fn main() {
    let _ = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded).model("model-only");
    let _ = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded).model_variant("low");
}

fn turn_builder_has_no_model_overlay(builder: lash::TurnBuilder) {
    let _ = builder.model("model-only");
}
