use super::*;
use lash::plugins::{OfferedTools, ProjectedHistoryStats, PromptCall, SectionText};
use lash::prompt::{PlacementSource, PromptPlacement, PromptPlan, PromptPurpose};
use lash::testing::prompt::{PromptCut, PromptCutParts, compose_sections};

fn workbench_plugin(factory: &WorkbenchPluginFactory) -> Arc<dyn SessionPlugin> {
    Arc::new(WorkbenchSessionPlugin {
        mail_world: factory.mail_world.clone(),
        config_changes: factory.config_changes.clone(),
        deferred_tools: factory.deferred_tools.clone(),
        approvals: factory.approvals.clone(),
        host_triggers: factory.host_triggers.clone(),
    })
}

fn cut(prompt: &WorkbenchPrompt, history_messages: u32) -> PromptCut {
    lash::testing::prompt::cut(PromptCutParts {
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
/// identity is its own section, not a protocol's; its standing instructions
/// and the connected accounts render from the workbench's own recorded
/// config, which a run is admitted under, in the instructions; the context budget states the call's projected history
/// late, outside the conversation; and a recorded prompt with no context
/// omits the accounts section instead of rendering an empty one.
#[test]
fn workbench_prompt_sections_render_its_recorded_host_text_and_the_context_budget() {
    let factory = WorkbenchPluginFactory::new();
    let catalog =
        lash::plugins::PromptCatalog::of_plugins(&[workbench_plugin(&factory)]).expect("catalog");
    let resolved = catalog
        .preview(
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            &OfferedTools::default(),
        )
        .expect("the empty plan resolves");
    assert_eq!(
        resolved
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
                "agent_workbench/intro".to_owned(),
                PromptPlacement::InitialInstructions,
                PlacementSource::PluginDefault
            ),
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
        compose_sections(
            &catalog,
            &PromptPlan::default(),
            &PromptPurpose::Turn,
            &cut(prompt, 3),
        )
        .expect("the sections render")
        .into_iter()
        .map(|section| section.value)
        .collect::<Vec<_>>()
    };
    assert_eq!(
        rendered(&recorded),
        vec![
            SectionText::text(WORKBENCH_INTRO),
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
    assert_eq!(rendered(&without_context)[2], SectionText::Omit);
}

/// The workbench's sections shape the prompt the provider receives (ADR
/// 0133): a Standard session's first call records each workbench section in
/// its snapshot: its standing instructions omitted (they are RLM's), the
/// connected accounts in the request's instructions and the context budget
/// late, after the conversation and outside the instructions. What the
/// snapshot records is what the provider was sent.
#[tokio::test]
async fn workbench_prompt_sections_shape_the_prompt_the_provider_receives() {
    const MODEL: &str = "workbench-prompt-model";
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds");
    let received = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let provider = {
        let received = Arc::clone(&received);
        lash::testing::TestProvider::builder()
            .kind("workbench-prompt")
            .complete(move |request| {
                let received = Arc::clone(&received);
                async move {
                    received.lock().expect("the request log").push((
                        request
                            .instructions
                            .as_deref()
                            .unwrap_or_default()
                            .to_owned(),
                        serde_json::to_string(&request.messages).expect("the messages encode"),
                    ));
                    Ok(lash::provider::LlmResponse {
                        parts: vec![lash::direct::LlmOutputPart::Text {
                            text: "done".to_owned(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let factory = WorkbenchPluginFactory::new();
    let mail_world = factory.mail_world.clone();
    let core = lash::LashCore::standard_builder(backend)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder(MODEL)
                .context_window_tokens(200_000)
                .build()
                .expect("the model's metadata"),
        )
        .plugin(Arc::new(factory))
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "workbench-prompt",
            "workbench-prompt-boot",
        ))
        .expect("the core builds");
    let spec = lash::SessionSpec::new(
        MODEL,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(12))
    .plugin(
        "agent_workbench",
        workbench_session_prompt(
            crate::session_protocol::SessionProtocol::Standard,
            &mail_world,
        ),
    )
    .expect("the workbench prompt records");
    core.session(lash::SessionId::from("workbench-prompt"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec,
        ))
        .await
        .expect("the session is created");
    let session = core
        .session(lash::SessionId::from("workbench-prompt"))
        .open()
        .await
        .expect("the session opens");
    let run = lash::TurnId::parse("workbench-prompt-turn").expect("run id");
    let turn = session
        .send(lash::TurnInput::text("hello"))
        .id(run.clone())
        .output()
        .await
        .expect("the turn answers");
    assert!(turn.is_success(), "{turn:?}");

    let loaded = session
        .admin()
        .prompt()
        .snapshot(&run, 1)
        .await
        .expect("the recorded snapshot reads through the facade")
        .expect("the call retains a snapshot");
    let recorded = |key: &str| {
        let section = loaded
            .snapshot
            .sections
            .iter()
            .find(|section| section.section.to_string() == format!("agent_workbench/{key}"))
            .unwrap_or_else(|| panic!("the call recorded agent_workbench/{key}"));
        (
            section.placement,
            loaded.text(&section.value).map(str::to_owned),
        )
    };
    let received = received.lock().expect("the request log").clone();
    let [(instructions, messages)] = received.as_slice() else {
        panic!("one model call: {received:?}");
    };
    assert_eq!(
        recorded(WORKBENCH_INSTRUCTIONS_SECTION),
        (PromptPlacement::InitialInstructions, None),
        "a Standard session's workbench instructions are an omission"
    );
    let (placement, accounts) = recorded(WORKBENCH_ACCOUNTS_SECTION);
    assert_eq!(placement, PromptPlacement::InitialInstructions);
    let accounts = accounts.expect("the accounts section recorded text");
    assert!(
        instructions.contains(&accounts),
        "the provider's instructions carry the recorded accounts text"
    );
    let (placement, budget) = recorded(WORKBENCH_CONTEXT_BUDGET_SECTION);
    let budget = budget.expect("the context budget recorded text");
    assert_eq!(placement, PromptPlacement::CurrentContext);
    assert!(budget.starts_with("Context budget:"), "{budget}");
    let encoded = serde_json::to_string(&budget).expect("the text encodes");
    assert!(
        messages.contains(encoded.trim_matches('"')) && !instructions.contains(&budget),
        "the provider receives the recorded context budget late, outside its instructions"
    );
    core.shutdown().await.expect("the core shuts down");
}
