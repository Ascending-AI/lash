use super::*;
use lash::plugins::{
    OfferedTools, ProjectedHistoryStats, PromptCall, PromptCut, PromptCutParts, SectionText,
};
use lash::prompt::{PlacementSource, PromptPlacement, PromptPlan, PromptPurpose};

fn workbench_plugin(factory: &WorkbenchPluginFactory) -> Arc<dyn SessionPlugin> {
    Arc::new(WorkbenchSessionPlugin {
        mail_world: factory.mail_world.clone(),
        config_changes: factory.config_changes.clone(),
        deferred_tools: factory.deferred_tools.clone(),
        approvals: factory.approvals.clone(),
    })
}

fn cut(prompt: &WorkbenchPrompt, history_messages: u32) -> PromptCut {
    PromptCut::new(PromptCutParts {
        call: PromptCall {
            session_id: lash::SessionId::from("workbench-prompt"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: PromptPurpose::Turn,
        },
        config: lash::plugins::AdmittedPluginConfig::new(
            serde_json::from_value(json!({
                "namespaces": {
                    "agent_workbench": { "format_version": 1, "value": prompt }
                }
            }))
            .expect("the workbench namespace records"),
            1,
        ),
        session: None,
        offered: OfferedTools::default(),
        model: lash::plugins::PromptModel::default(),
        history: ProjectedHistoryStats {
            messages: history_messages,
            estimated_tokens: 0,
        },
        namespaces: BTreeMap::new(),
    })
}

/// The workbench's model-facing text is prompt sections (FIG-5258, ADR 0133),
/// replacing its context transform and its protocol prompt config. Its
/// standing instructions and the connected accounts render from the
/// workbench's own recorded config, which a run is admitted under, in the
/// instructions; the context budget states the call's projected history
/// late, outside the conversation; and a recorded prompt with no context
/// omits the accounts section instead of rendering an empty one.
#[test]
fn workbench_prompt_sections_render_its_recorded_host_text_and_the_context_budget() {
    let factory = WorkbenchPluginFactory::new();
    let catalog =
        lash::plugins::PromptCatalog::of_plugins(&[workbench_plugin(&factory)]).expect("catalog");
    let resolved = catalog
        .resolve(
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            &OfferedTools::default(),
        )
        .expect("the empty plan resolves");
    assert_eq!(
        resolved
            .record()
            .sections
            .iter()
            .map(|section| (
                section.section.to_string(),
                section.placement,
                section.placement_source
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                format!("agent_workbench/{WORKBENCH_INSTRUCTIONS_SECTION}"),
                PromptPlacement::InitialInstructions,
                PlacementSource::PluginDefault
            ),
            (
                format!("agent_workbench/{WORKBENCH_ACCOUNTS_SECTION}"),
                PromptPlacement::InitialInstructions,
                PlacementSource::PluginDefault
            ),
            (
                format!("agent_workbench/{WORKBENCH_CONTEXT_BUDGET_SECTION}"),
                PromptPlacement::CurrentContext,
                PlacementSource::PluginDefault
            ),
        ]
    );

    let recorded = workbench_session_prompt(
        crate::session_protocol::SessionProtocol::Rlm,
        &factory.mail_world,
    );
    let rendered = |prompt: &WorkbenchPrompt| {
        let cut = cut(prompt, 3);
        (0..3)
            .map(|index| {
                resolved
                    .compose_section(index, &cut)
                    .expect("the section renders")
                    .value
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rendered(&recorded),
        vec![
            SectionText::Text(format!(
                "{}\n\n{}",
                workbench_prompt().trim(),
                deferred_tools::prompt_preview().trim()
            )),
            SectionText::Text(connected_accounts_prompt(&factory.mail_world)),
            SectionText::text("Context budget: prepared 3 message(s) from 0 committed"),
        ]
    );
    let without_context = WorkbenchPrompt {
        context: Vec::new(),
        ..recorded
    };
    assert_eq!(rendered(&without_context)[1], SectionText::Omit);
}
