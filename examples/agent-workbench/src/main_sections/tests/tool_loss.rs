//! The workbench tells its user when a run lost a tool (FIG-3367, FIG-5134).

use super::*;
use lash::tools::ToolDefinitionBindingExt as _;

fn lost_tool_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:workbench_seed_lookup",
        "workbench_seed_lookup",
        "a host tool only the seeding workbench serves",
        lash::tools::ToolDefinition::default_input_schema(),
        json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash::tools::ToolBinding::new(["workbench"], "seed_lookup"))
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

/// The system rows the workbench published.
fn system_rows(state: &AppState) -> Vec<String> {
    state
        .messages_snapshot()
        .into_iter()
        .filter(|message| message.role == "system")
        .map(|message| message.text)
        .collect()
}

/// A run whose session's recorded tool has no source in this workbench is
/// rendered to the user as one chat row naming the tool, not swallowed into
/// the log: the seeding workbench serves the tool and its run records it; a
/// restarted workbench over the same stores does not, so its run reports the
/// loss and the turn's follower renders it. Another run reporting the same
/// loss does not repeat the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_lost_a_tool_renders_the_loss_to_the_user() {
    let seeding = Workbench::builder(replying_provider("done"))
        .tool_provider(Arc::new(SeedTools))
        .build()
        .await;
    let session_id = seeding.state.current_session_id();
    run_turn(&seeding.state, "record the tool").await;
    assert!(
        system_rows(&seeding.state).is_empty(),
        "a run that has its tools reports no loss"
    );
    let stores = Arc::clone(&seeding.stores);
    seeding.shutdown().await;

    let workbench = Workbench::builder(replying_provider("done"))
        .stores(stores)
        .build()
        .await;
    let state = &workbench.state;
    run_turn_in(state, &session_id, "run without the tool").await;
    let rendered = system_rows(state);
    let [row] = rendered.as_slice() else {
        panic!("the user is told exactly once, got {rendered:?}");
    };
    assert!(
        row.contains("tool:workbench_seed_lookup"),
        "the rendered row names the lost tool: {row}"
    );

    run_turn_in(state, &session_id, "run without the tool again").await;
    assert_eq!(
        system_rows(state).len(),
        1,
        "one row per distinct loss, not one per run"
    );
    workbench.shutdown().await;
}
