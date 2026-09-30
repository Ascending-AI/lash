fn main() {
    let mut context = lash::runtime::TurnContext::new();
    context.set_prompt_layer(lash::prompt::PromptLayer::new());
    context.mark_selected_queued_work_drain();
}
