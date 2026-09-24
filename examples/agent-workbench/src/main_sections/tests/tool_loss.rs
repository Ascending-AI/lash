//! The workbench tells its user when an open lost a tool (FIG-3367).

use super::*;

fn lost_tool_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:workbench_seed_lookup",
        "workbench_seed_lookup",
        "a host tool only the seeding core carries",
        lash::tools::ToolDefinition::default_input_schema(),
        json!({ "type": "object", "additionalProperties": true }),
    )
}

struct SeedTools;

#[async_trait]
impl lash::tools::ToolProvider for SeedTools {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        vec![lost_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        (name == "workbench_seed_lookup").then(|| Arc::new(lost_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(json!({ "ok": true })).into()
    }
}

/// A session whose persisted tool has no source here is rendered to the user
/// as a chat row naming the tool, not swallowed into the workbench log.
///
/// The seeding core carries the tool source and commits a checkpoint with it;
/// the workbench's own core does not. Dropping the restore report — or
/// rendering only the policy value instead of the report — leaves
/// `messages_snapshot()` without the row and fails here.
#[tokio::test]
async fn an_open_that_lost_a_tool_renders_the_loss_to_the_user() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-tool-loss-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
    let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
    );
    let session_id = lash::SessionId::from("workbench-tool-loss");

    // Seed a checkpoint that records the tool, on a core that has its source.
    let seeding_core = explicit_durable_test_facets(&data_dir)
        .provider(
            lash::testing::TestProvider::builder()
                .kind("workbench-test")
                .complete_error("the seed never calls the provider")
                .build()
                .into_handle(),
        )
        .model(test_model())
        .tools(Arc::new(SeedTools))
        .build(crate::test_core_owner())
        .expect("build the seeding core");
    let seeded = seeding_core
        .session(session_id.clone())
        .open()
        .await
        .expect("seed open");
    seeded
        .admin()
        .state()
        .append_messages(vec![lash::plugins::PluginMessage::text(
            lash::messages::MessageRole::Assistant,
            "seeded while the tool source was present",
        )])
        .await
        .expect("commit a checkpoint carrying the tool");
    seeded.close().await.expect("close the seeded session");

    // The workbench's own core has no such source.
    let core = explicit_durable_test_facets(&data_dir)
        .provider(
            lash::testing::TestProvider::builder()
                .kind("workbench-test")
                .complete_error("this test never calls the provider")
                .build()
                .into_handle(),
        )
        .model(test_model())
        .build(crate::test_core_owner())
        .expect("build core");
    let process_observer = core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&core_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        sessions: WorkbenchSessions::fresh(),
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_model: Arc::new(Mutex::new(ModelSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(16),
        queued_work_driver: inert_queued_work(),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };

    let opened = state
        .open_session(&session_id, "tool-loss-test")
        .await
        .expect("the session still opens: the default policy tolerates loss");
    opened.close().await.expect("close");

    let rendered = state
        .messages_snapshot()
        .into_iter()
        .filter(|message| message.role == "system")
        .map(|message| message.text)
        .collect::<Vec<_>>();
    assert_eq!(
        rendered.len(),
        1,
        "the user is told exactly once, got {rendered:?}"
    );
    assert!(
        rendered[0].contains("tool:workbench_seed_lookup"),
        "the rendered row names the lost tool id: {}",
        rendered[0]
    );

    // Opening again does not repeat the row: the id is derived from the loss.
    let again = state
        .open_session(&session_id, "tool-loss-test")
        .await
        .expect("second open");
    again.close().await.expect("close");
    assert_eq!(
        state
            .messages_snapshot()
            .into_iter()
            .filter(|message| message.role == "system")
            .count(),
        1,
        "one row per distinct loss, not one per open"
    );

    let _ = std::fs::remove_dir_all(data_dir);
}
