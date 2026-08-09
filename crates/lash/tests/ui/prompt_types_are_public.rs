use lash::PromptLayerSink;

fn main() {
    let template = lash::prompt::PromptTemplate::new(vec![
        lash::prompt::PromptTemplateSection::untitled(vec![
            lash::prompt::PromptTemplateEntry::builtin(
                lash::prompt::PromptBuiltin::MainAgentIntro,
            ),
            lash::prompt::PromptTemplateEntry::slot(lash::prompt::PromptSlot::Guidance),
        ]),
    ]);
    let contribution =
        lash::prompt::PromptContribution::guidance("Host", "Host guidance").with_priority(1);
    let layer = lash::prompt::PromptLayer::with_template(template)
        .with_contribution(contribution)
        .with_cleared_slot(lash::prompt::PromptSlot::Environment);

    let _ = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded).prompt_layer(layer);
    let _ = lash::prompt::default_prompt_template();
}
