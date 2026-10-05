use super::*;

/// FIG-5004: every tool the workbench hands a provider on a Standard turn must
/// resolve under every provider preset's tool-input dialects. XOR-style
/// constraints belong in tool input validation, not in schema combinators the
/// dialects reject.
#[test]
fn every_tool_a_standard_workbench_turn_offers_projects_under_every_provider_tool_dialect() {
    run_async_test_on_stack_budget("workbench-provider-tool-projection", || {
        every_tool_a_standard_workbench_turn_offers_projects_under_every_provider_tool_dialect_inner(
        )
    });
}

async fn every_tool_a_standard_workbench_turn_offers_projects_under_every_provider_tool_dialect_inner()
 {
    let offered = Arc::new(Mutex::new(None::<Arc<Vec<lash::plugins::LlmToolSpec>>>));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete({
            let offered = Arc::clone(&offered);
            move |request| {
                let offered = Arc::clone(&offered);
                async move {
                    *offered.lock_recover() = Some(request.tools);
                    Ok(text_response("done"))
                }
            }
        })
        .build()
        .into_handle();
    let mail_world = mail::MailWorld::new();
    mail_world.add_account("test").expect("add test account");
    let subagent_registry = Arc::new(lash::subagents::default_registry(&BTreeMap::new()));
    let deferred_tools =
        deferred_tools::WorkbenchDeferredTools::in_memory().expect("open deferred-tool grants");
    let approvals = approvals::WorkbenchApprovals::in_memory().expect("open approval ledger");
    let mcp = Arc::new(lash::mcp::McpPluginFactory::empty());
    let double = crate::tests::test_double_backend(0).await;
    let core = lash::LashCore::standard_builder(double.lash_backend())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_workbench_llm_profile(provider, test_llm_profile())
        .configure_plugins(move |plugins| {
            configure_workbench_plugins(
                plugins,
                mail_world,
                subagent_registry,
                deferred_tools,
                approvals,
                mcp,
            );
        })
        .build(crate::test_core_owner())
        .expect("build core");
    let session = crate::created_session(&core, WorkbenchSessions::fresh().current())
        .await
        .open()
        .await
        .expect("open session");
    session
        .send(lash::TurnInput::text("report the offered tool surface"))
        .id(lash::TurnId::fixture(format!(
            "workbench-tool-projection:{}",
            uuid::Uuid::new_v4()
        )))
        .output()
        .await
        .expect("the standard turn completes");

    let tools = offered
        .lock_recover()
        .clone()
        .expect("the provider observed a request");
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        names.contains(&"start_process"),
        "the offered tool surface must include start_process: {names:?}"
    );
    assert!(
        names.contains(&"inbox__test__send"),
        "the offered tool surface must include inbox__test__send: {names:?}"
    );

    let presets = [
        (
            "openai",
            lash::schema::ProviderSchemaCapabilities::openai(false),
        ),
        (
            "openai-strict",
            lash::schema::ProviderSchemaCapabilities::openai(true),
        ),
        (
            "anthropic",
            lash::schema::ProviderSchemaCapabilities::anthropic(),
        ),
        (
            "bedrock-claude",
            lash::schema::ProviderSchemaCapabilities::bedrock_claude(),
        ),
        ("google", lash::schema::ProviderSchemaCapabilities::google()),
    ];
    let mut failures = Vec::new();
    for tool in tools.iter() {
        for (preset, capabilities) in &presets {
            if let Err(error) = lash::schema::resolve_schema(
                &tool.input_schema,
                lash::schema::SchemaResolutionRequest {
                    provider: preset,
                    purpose: lash::schema::SchemaPurpose::ToolInput,
                    dialects: capabilities.dialects_for(lash::schema::SchemaPurpose::ToolInput),
                },
            ) {
                failures.push(format!(
                    "{} / {}: {}",
                    tool.name,
                    preset,
                    error.diagnostics.join("; ")
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "every offered tool must project under every provider tool dialect:\n{}",
        failures.join("\n")
    );
}
