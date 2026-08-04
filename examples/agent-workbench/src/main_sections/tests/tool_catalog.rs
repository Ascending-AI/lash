fn catalog_lifecycle_provider() -> lash::provider::ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-test")
        .complete(move |_| {
            let calls = Arc::clone(&calls);
            async move {
                let account = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => "test",
                    1 => "live",
                    call => panic!("unexpected workbench provider call {call}"),
                };
                Ok(text_response(&format!(
                    "<lashlang>\nresult = await inbox.{account}.send({{ title: \"Hi\", text: \"Yo\" }})?\nfinish result.id\n</lashlang>"
                )))
            }
        })
        .build()
        .into_handle()
}

async fn assert_tool_catalog_contract(
    core: &lash::LashCore,
    session: &lash::LashSession,
) {
    let core_catalog: lash::ToolCatalogView = core.tool_catalog();
    let core_manifests = core_catalog.manifests();
    assert!(
        core_manifests
            .iter()
            .any(|manifest| manifest.name == "inbox__test__send"),
        "core catalog composes the workbench plugin's inbox tool"
    );
    let send_manifest = core_manifests
        .iter()
        .find(|manifest| manifest.name == "inbox__test__send")
        .expect("core catalog includes inbox.test send")
        .clone();
    let core_contract = core_catalog
        .resolve_contract("inbox__test__send")
        .expect("core catalog resolves inbox.test send");
    assert_eq!(
        core_contract.input_schema.canonical()["required"],
        serde_json::json!(["title"]),
        "core catalog exposes the runtime input schema"
    );
    assert!(
        matches!(
            core_contract.output_contract,
            lash::tools::ToolOutputContract::Static
        ),
        "core catalog exposes the runtime output contract"
    );
    let miss: lash::ToolCatalogMiss = core_catalog
        .resolve_contract("inbox__test__missing")
        .expect_err("unknown core tool has a typed miss");
    assert_eq!(miss.name, "inbox__test__missing");

    let session_tools = session.tools();
    let session_contract = session_tools
        .resolve_contract("inbox__test__send")
        .await
        .expect("session catalog resolves an active tool");
    assert_eq!(
        serde_json::to_value(session_contract.as_ref()).expect("serialize session contract"),
        serde_json::to_value(core_contract.as_ref()).expect("serialize core contract"),
        "core and initial session projections agree"
    );

    session_tools
        .set_membership(send_manifest.id.clone(), false)
        .await
        .expect("remove send from this session catalog");
    let gated = session_tools
        .resolve_contract("inbox__test__send")
        .await
        .expect_err("non-member tool must resolve as a typed miss in this session");
    assert_eq!(gated.name, "inbox__test__send");
    assert!(
        core_catalog.resolve_contract("inbox__test__send").is_ok(),
        "session catalog curation must not mutate the core projection"
    );
    session_tools
        .set_membership(send_manifest.id, true)
        .await
        .expect("restore send to this session catalog");
}

async fn assert_plugin_provider_execution(
    session: &lash::LashSession,
    plugin_mail_world: &mail::MailWorld,
) {
    let output = session
        .turn(lash::TurnInput::text("send through the plugin provider"))
        .turn_id(format!("workbench-test-turn:{}", uuid::Uuid::new_v4()))
        .run()
        .await
        .expect("turn should resolve and execute inbox.test.send");
    assert_eq!(output.final_value(), Some(&serde_json::json!("test-1")));
    assert_eq!(plugin_mail_world.inbox("test").expect("test inbox").len(), 1);
}

struct LiveProviderFixture {
    source: lash::tools::ToolSourceHandle,
    source_id: String,
    tool_id: lash::tools::ToolId,
    mail_world: mail::MailWorld,
}

async fn add_live_provider(
    core: &lash::LashCore,
    session: &lash::LashSession,
) -> LiveProviderFixture {
    let core_catalog = core.tool_catalog();
    let session_tools = session.tools();

    let live_mail_world = mail::MailWorld::new();
    live_mail_world
        .add_account("live")
        .expect("add live-provider account");
    let live_source = session_tools
        .add_provider(Arc::new(mail::MockMailProvider::new(
            live_mail_world.clone(),
        )))
        .await
        .expect("add a provider to the live session catalog");
    let live_source_id = live_source.id().to_string();
    let live_contract = session_tools
        .resolve_contract("inbox__live__send")
        .await
        .expect("session resolves a tool from the live provider");
    assert_eq!(
        live_contract.input_schema.canonical()["required"],
        serde_json::json!(["title"]),
        "session resolution reflects the live provider contract"
    );
    assert_eq!(
        live_source.id(),
        live_source_id,
        "the source identity is stable across add and resolve"
    );
    let live_manifest = session_tools
        .active_manifests()
        .await
        .expect("read active session manifests")
        .into_iter()
        .find(|manifest| manifest.name == "inbox__live__send")
        .expect("the added provider is immediately visible to the model catalog");
    assert!(
        core_catalog
            .resolve_contract("inbox__live__send")
            .is_err(),
        "session provider mutation must not change the core catalog projection"
    );

    LiveProviderFixture {
        source: live_source,
        source_id: live_source_id,
        tool_id: live_manifest.id,
        mail_world: live_mail_world,
    }
}

async fn assert_live_tool_provider_execution_and_removal(
    core: &lash::LashCore,
    session: &lash::LashSession,
) {
    let live = add_live_provider(core, session).await;
    let output = session
        .turn(lash::TurnInput::text("send through the live provider"))
        .turn_id(format!("workbench-test-turn:{}", uuid::Uuid::new_v4()))
        .run()
        .await
        .expect("turn should resolve and execute inbox.live.send");
    assert_eq!(output.final_value(), Some(&serde_json::json!("live-1")));
    assert_eq!(
        live.mail_world.inbox("live").expect("live inbox").len(),
        1
    );

    let membership_generation = session
        .tools()
        .set_membership(live.tool_id.clone(), false)
        .await
        .expect("record a source membership choice before removal");
    let removal_generation = session
        .tools()
        .remove_source(&live.source)
        .await
        .expect("remove the live provider source");
    assert!(
        removal_generation > membership_generation,
        "removal must advance the generation from the preceding state mutation"
    );
    let removal_state = session.tools().state().await.expect("state after removal");
    assert_eq!(
        serde_json::to_value(removal_state).expect("serialize tool state")["generation"],
        serde_json::json!(removal_generation),
        "remove_source returns the refreshed session ToolState generation"
    );
    let removed = session
        .tools()
        .resolve_contract("inbox__live__send")
        .await
        .expect_err("removed live-provider tool must miss in subsequent resolution");
    assert_eq!(removed.name, "inbox__live__send");
    assert!(
        core.tool_catalog()
            .resolve_contract("inbox__live__send")
            .is_err(),
        "the core projection remains unchanged after session source removal"
    );
    let absent_source = session
        .tools()
        .remove_source(&live.source)
        .await
        .expect_err("removing an absent source must remain an error");
    assert!(
        absent_source.to_string().contains(&live.source_id),
        "the removal error names source {}: {absent_source}",
        live.source_id
    );

    let replacement_source = session
        .tools()
        .add_provider(Arc::new(mail::MockMailProvider::new(
            live.mail_world.clone(),
        )))
        .await
        .expect("re-add the live provider");
    assert!(
        session
            .tools()
            .active_manifests()
            .await
            .expect("active manifests after re-add")
            .iter()
            .any(|manifest| manifest.name == "inbox__live__send"),
        "re-adding a removed source creates fresh default-member tool state"
    );
    session
        .tools()
        .remove_source(&replacement_source)
        .await
        .expect("clean up the replacement live provider");
}
