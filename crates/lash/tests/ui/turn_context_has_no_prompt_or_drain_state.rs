// Instruction text is a prompt section (ADR 0133): a turn context carries
// no prompt, and no drain selection.
fn main() {
    let mut context = lash::runtime::TurnContext::new();
    context.set_system_prompt("a prompt for this turn");
    context.mark_selected_queued_work_drain();
}
