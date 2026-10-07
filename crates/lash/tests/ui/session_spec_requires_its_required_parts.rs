// FIG-4594: a root creation states its whole spec. `SessionSpec::new` takes
// the model key, the turn budget and the tool-call limit, so a spec that
// leaves one out does not compile, and neither a spec nor a creation has a
// default to fall back on.

fn a_spec_names_its_required_parts() {
    let _ = lash::SessionSpec::new();
}

fn a_spec_names_its_turn_budget(model: lash::LlmProfileKey) {
    let _ = lash::SessionSpec::new(model);
}

fn a_spec_names_its_tool_call_limit(model: lash::LlmProfileKey) {
    let _ = lash::SessionSpec::new(model, lash::TurnBudget::Unbounded);
}

fn a_spec_has_no_default() {
    let _ = lash::SessionSpec::default();
}

fn a_creation_has_no_default() {
    let _ = lash::SessionCreation::default();
}

fn a_creation_states_its_spec() {
    let _ = lash::SessionCreation {
        parent: None,
        prompt_plan: None,
    };
}

fn main() {}
