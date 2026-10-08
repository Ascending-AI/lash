use super::*;

/// FIG-5004: every tool the workbench hands a provider on a Standard turn
/// resolves under every provider preset's tool-input dialects. XOR-style
/// constraints belong in tool input validation, not in schema combinators
/// the dialects reject. The core is the workbench's Standard composition: its
/// plugin stack over a durable SQLite memory backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_a_standard_workbench_turn_offers_projects_under_every_provider_tool_dialect() {
    let offered = Arc::new(Mutex::new(None::<Arc<Vec<lash::plugins::LlmToolSpec>>>));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete({
            let offered = Arc::clone(&offered);
            move |request| {
                *offered.lock_recover() = Some(request.tools);
                async { Ok(text_response("done")) }
            }
        })
        .build()
        .into_handle();
    let mail_world = mail::MailWorld::new();
    mail_world
        .add_account("test")
        .expect("add the test account");
    let session_defaults = workbench_session_defaults(
        &LlmProfileSelection {
            model: TEST_MODEL.to_string(),
            model_variant: None,
        },
        None,
    );
    let delegation: Arc<dyn PluginFactory> = Arc::new(delegation::DelegationPluginFactory::new(
        session_defaults.clone(),
        lash::process::lifetime::starter,
    ));
    let deferred_tools =
        deferred_tools::WorkbenchDeferredTools::in_memory().expect("open the deferred-tool grants");
    let approvals = approvals::WorkbenchApprovals::in_memory().expect("open the approval ledger");
    let mcp: Arc<dyn PluginFactory> = Arc::new(
        lash::mcp::McpPluginFactory::builder(BTreeMap::new())
            .build()
            .await
            .expect("an MCP factory with no servers"),
    );
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .expect("the durable backend builds");
    let core = LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .llm_profiles(Arc::new(WorkbenchLlmProfiles { provider }))
        .configure_plugins(move |plugins| {
            configure_workbench_plugins(
                plugins,
                mail_world,
                delegation,
                deferred_tools,
                approvals,
                host_triggers::HostTriggers::in_memory().expect("open the trigger tables"),
                mcp,
            );
        })
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "agent-workbench-test",
            uuid::Uuid::new_v4().to_string(),
        ))
        .expect("build the Standard workbench core");
    let session_id = lash::SessionId::from("workbench-tool-projection");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(session_defaults))
        .await
        .expect("create the session");
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("open the session");
    session
        .send(lash::TurnInput::text("report the offered tool surface"))
        .output()
        .await
        .expect("the Standard turn answers");

    let tools = offered
        .lock_recover()
        .clone()
        .expect("the provider observed a request");
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["start_process", "inbox__test__send"] {
        assert!(
            names.contains(&expected),
            "the offered tool surface must include {expected}: {names:?}"
        );
    }

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
    drop(session);
    core.shutdown().await.expect("the core shuts down");
}
