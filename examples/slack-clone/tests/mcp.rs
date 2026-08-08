use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use lash::direct::LlmOutputPart;
use lash::provider::{LlmResponse, ProviderHandle};
use lash::tools::{ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolProvider, ToolResult};
use lash::{ModelSpec, TurnInput};
use lash_plugin_mcp::McpServerConfig;
use serde_json::{Value, json};
use slack_clone::bot::runtime::{self, RuntimeConfig};
use slack_clone::bot::slack_api::SlackApi;
use slack_clone::mcp_server::{
    API_BASE_URL_ENV, BOT_TOKEN_ENV, LIST_CHANNELS_SUMMARY_TOOL, WORKSPACE_STATS_TOOL,
};
use tokio::sync::Notify;

const TEST_TOKEN: &str = "mcp-integration-test-token";

#[derive(Clone)]
struct FakeApiState {
    block_next_channels_call: Arc<AtomicBool>,
    channels_call_entered: Arc<Notify>,
}

impl FakeApiState {
    fn normal() -> Self {
        Self {
            block_next_channels_call: Arc::new(AtomicBool::new(false)),
            channels_call_entered: Arc::new(Notify::new()),
        }
    }

    fn block_next() -> Self {
        Self {
            block_next_channels_call: Arc::new(AtomicBool::new(true)),
            channels_call_entered: Arc::new(Notify::new()),
        }
    }
}

async fn conversations_list(State(state): State<FakeApiState>) -> Json<Value> {
    if state.block_next_channels_call.swap(false, Ordering::SeqCst) {
        state.channels_call_entered.notify_one();
        std::future::pending::<()>().await;
    }
    Json(json!({
        "ok": true,
        "channels": [{
            "id": "CDEMO",
            "name": "engineering",
            "is_channel": true,
            "is_group": false,
            "is_im": false,
            "created": 1,
            "creator": "UADA",
            "is_archived": false,
            "is_general": true,
            "name_normalized": "engineering",
            "is_member": true,
            "is_private": false,
            "is_mpim": false,
            "topic": { "value": "Build the product", "creator": "UADA", "last_set": 1 },
            "purpose": { "value": "Engineering", "creator": "UADA", "last_set": 1 },
            "num_members": 2
        }],
        "response_metadata": { "next_cursor": "" }
    }))
}

async fn users_list() -> Json<Value> {
    Json(json!({
        "ok": true,
        "members": [
            user("UADA", "ada", false, false),
            user("UBOT", "lashbot", true, false),
            user("UOLD", "former-member", false, true)
        ],
        "cache_ts": 1,
        "response_metadata": { "next_cursor": "" }
    }))
}

fn user(id: &str, name: &str, is_bot: bool, deleted: bool) -> Value {
    json!({
        "id": id,
        "team_id": "TDEMO",
        "name": name,
        "deleted": deleted,
        "color": "000000",
        "real_name": name,
        "tz": "UTC",
        "tz_label": "UTC",
        "tz_offset": 0,
        "profile": {
            "real_name": name,
            "display_name": name,
            "real_name_normalized": name,
            "display_name_normalized": name,
            "team": "TDEMO"
        },
        "is_admin": false,
        "is_owner": false,
        "is_primary_owner": false,
        "is_restricted": false,
        "is_ultra_restricted": false,
        "is_bot": is_bot,
        "updated": 1,
        "is_app_user": is_bot
    })
}

async fn fake_api(state: FakeApiState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake API");
    let addr = listener.local_addr().expect("fake API address");
    let app = Router::new()
        .route("/api/conversations.list", post(conversations_list))
        .route("/api/users.list", post(users_list))
        .with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve fake API");
    });
    (format!("http://{addr}"), server)
}

#[derive(Clone)]
enum Step {
    Tool(&'static str),
    Text(&'static str),
}

#[derive(Clone)]
struct Script {
    steps: Arc<tokio::sync::Mutex<VecDeque<Step>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(tokio::sync::Mutex::new(steps.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn provider(&self) -> ProviderHandle {
        let steps = Arc::clone(&self.steps);
        let requests = Arc::clone(&self.requests);
        lash::testing::TestProvider::builder()
            .kind("slack-clone-mcp-test")
            .complete(move |request| {
                let steps = Arc::clone(&steps);
                let requests = Arc::clone(&requests);
                async move {
                    requests
                        .lock()
                        .expect("request lock")
                        .push(serde_json::to_string(&request).expect("serialize request"));
                    let step = steps.lock().await.pop_front().unwrap_or(Step::Text("done"));
                    Ok(match step {
                        Step::Tool(name) => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "mcp-call".to_string(),
                                tool_name: name.to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        },
                        Step::Text(text) => LlmResponse {
                            full_text: text.to_string(),
                            parts: vec![LlmOutputPart::Text {
                                text: text.to_string(),
                                response_meta: None,
                            }],
                            ..LlmResponse::default()
                        },
                    })
                }
            })
            .build()
            .into_handle()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }
}

fn direct_server_config(api_base_url: &str) -> McpServerConfig {
    McpServerConfig::Stdio {
        command: env!("CARGO_BIN_EXE_slack-clone-mcp-server").to_string(),
        args: Vec::new(),
        env: BTreeMap::from([
            (API_BASE_URL_ENV.to_string(), api_base_url.to_string()),
            (BOT_TOKEN_ENV.to_string(), TEST_TOKEN.to_string()),
        ]),
        cwd: None,
        startup_timeout_ms: 5_000,
        call_timeout_ms: 5_000,
        binary_content_attachments: false,
    }
}

fn wrapped_server_config(api_base_url: &str, pid_file: &std::path::Path) -> McpServerConfig {
    McpServerConfig::Stdio {
        command: "sh".to_string(),
        args: vec![
            "-c".to_string(),
            "printf '%s\\n' \"$$\" > \"$MCP_PID_FILE\"; exec \"$MCP_BINARY\"".to_string(),
        ],
        env: BTreeMap::from([
            (API_BASE_URL_ENV.to_string(), api_base_url.to_string()),
            (BOT_TOKEN_ENV.to_string(), TEST_TOKEN.to_string()),
            (
                "MCP_BINARY".to_string(),
                env!("CARGO_BIN_EXE_slack-clone-mcp-server").to_string(),
            ),
            ("MCP_PID_FILE".to_string(), pid_file.display().to_string()),
        ]),
        cwd: None,
        startup_timeout_ms: 5_000,
        call_timeout_ms: 5_000,
        binary_content_attachments: false,
    }
}

async fn build_core(
    root: &std::path::Path,
    api_base_url: &str,
    script: &Script,
    server: McpServerConfig,
) -> lash::LashCore {
    let mut config = RuntimeConfig::new(root);
    config.trace_to_stderr = false;
    config.mcp_servers.insert("slack_clone".to_string(), server);
    let api = Arc::new(SlackApi::new(api_base_url, TEST_TOKEN).expect("build API client"));
    let model = ModelSpec::builder("mock/model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid model");
    runtime::build_core(&config, script.provider(), model, api)
        .await
        .expect("build MCP-enabled core")
}

fn transcript_text(session: &lash::LashSession) -> String {
    session
        .read_view()
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                Some(lash::message_text(&message))
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn bundled_mcp_tools_join_the_catalog_and_feed_the_standard_tool_loop() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let state = FakeApiState::normal();
    let (api_base_url, _server) = fake_api(state).await;
    let script = Script::new([
        Step::Tool(LIST_CHANNELS_SUMMARY_TOOL),
        Step::Text("The workspace has an engineering channel."),
    ]);
    let core = build_core(
        scratch.path(),
        &api_base_url,
        &script,
        direct_server_config(&api_base_url),
    )
    .await;
    let session = core
        .session("mcp-catalog")
        .open()
        .await
        .expect("open session");

    let names = session
        .tools()
        .active_manifests()
        .await
        .expect("read catalog")
        .into_iter()
        .map(|manifest| manifest.name)
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name == "list_channels"));
    assert!(names.iter().any(|name| name == LIST_CHANNELS_SUMMARY_TOOL));
    assert!(names.iter().any(|name| name == WORKSPACE_STATS_TOOL));

    session
        .turn(TurnInput::text("@lashbot summarize the workspace"))
        .run()
        .await
        .expect("run MCP turn");
    let requests = script.requests();
    assert_eq!(requests.len(), 2, "one tool call and one final answer");
    assert!(
        requests[1].contains("engineering") && requests[1].contains("Build the product"),
        "the live MCP result must reach the next model request: {}",
        requests[1]
    );
    let transcript = transcript_text(&session);
    assert!(
        transcript.contains("engineering") && transcript.contains("Build the product"),
        "the committed transcript must retain the MCP result: {transcript}"
    );
}

#[tokio::test]
async fn server_death_is_a_typed_failure_and_the_next_turn_uses_a_respawned_server() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let state = FakeApiState::block_next();
    let entered = Arc::clone(&state.channels_call_entered);
    let (api_base_url, _server) = fake_api(state).await;
    let pid_file = scratch.path().join("mcp.pid");
    let script = Script::new([
        Step::Tool(WORKSPACE_STATS_TOOL),
        Step::Text("The MCP call failed cleanly."),
        Step::Tool(WORKSPACE_STATS_TOOL),
        Step::Text("The recovered MCP server reports two members."),
    ]);
    let core = build_core(
        &scratch.path().join("lash"),
        &api_base_url,
        &script,
        wrapped_server_config(&api_base_url, &pid_file),
    )
    .await;
    let session = core
        .session("mcp-recovery")
        .open()
        .await
        .expect("open session");
    let original_pid = read_pid(&pid_file).await;

    let first = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .turn(TurnInput::text("@lashbot count the workspace"))
                .run()
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("workspace_stats reached the platform API");
    kill_process(original_pid);
    let first_turn = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("turn must not hang")
        .expect("turn task must not panic")
        .expect("the tool failure is model-visible, not a turn failure");

    let failure = first_turn
        .result
        .tool_calls
        .iter()
        .map(|call| call.output.value_for_projection())
        .find(|output| output.get("class").is_some())
        .expect("the failed MCP call must retain structured failure evidence");
    assert_eq!(failure["class"], "unavailable");
    assert_eq!(failure["code"], "mcp_connection_lost");

    let requests = script.requests();
    assert!(
        requests[1].contains("Tool execution failed"),
        "the next model request must receive the transport failure: {}",
        requests[1]
    );

    let replacement_pid = wait_for_replacement_pid(&pid_file, original_pid).await;
    assert_ne!(replacement_pid, original_pid, "the stdio pool must respawn");
    tokio::time::sleep(Duration::from_millis(250)).await;
    session
        .turn(TurnInput::text("@lashbot try the count again"))
        .run()
        .await
        .expect("next turn recovers");
    let requests = script.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[3].contains("\\\"active_members\\\":2")
            && requests[3].contains("\\\"channels\\\":1"),
        "the respawned server result reaches the next turn: {}",
        requests[3]
    );
}

async fn read_pid(path: &std::path::Path) -> u32 {
    for _ in 0..100 {
        if let Ok(value) = std::fs::read_to_string(path)
            && let Ok(pid) = value.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("MCP pid file was not written: {}", path.display());
}

async fn wait_for_replacement_pid(path: &std::path::Path, original: u32) -> u32 {
    for _ in 0..150 {
        let pid = read_pid(path).await;
        if pid != original {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("MCP server was not respawned");
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("invoke kill");
    assert!(status.success(), "kill MCP child");
}

#[cfg(not(unix))]
fn kill_process(_pid: u32) {
    panic!("server-death integration test requires Unix process signals");
}

struct CollidingTool;

#[async_trait]
impl ToolProvider for CollidingTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![collision_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == WORKSPACE_STATS_TOOL).then(|| Arc::new(collision_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
        ToolResult::ok(json!({ "wrong": true }))
    }
}

fn collision_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:native_collision",
        WORKSPACE_STATS_TOOL,
        "A deliberately colliding native tool",
        json!({ "type": "object", "properties": {} }),
        json!({}),
    )
}

#[tokio::test]
async fn an_exact_native_name_collision_is_rejected_instead_of_shadowing_mcp() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let state = FakeApiState::normal();
    let (api_base_url, _server) = fake_api(state).await;
    let script = Script::new([Step::Text("unused")]);
    let core = build_core(
        scratch.path(),
        &api_base_url,
        &script,
        direct_server_config(&api_base_url),
    )
    .await;
    let session = core
        .session("mcp-collision")
        .open()
        .await
        .expect("open session");
    let error = session
        .tools()
        .add_provider(Arc::new(CollidingTool))
        .await
        .expect_err("ordinary live-source collisions must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("duplicate tool name") && message.contains(WORKSPACE_STATS_TOOL),
        "collision error must name the policy and tool: {message}"
    );
    let names = session
        .tools()
        .active_manifests()
        .await
        .expect("read catalog after rejected collision");
    assert_eq!(
        names
            .iter()
            .filter(|manifest| manifest.name == WORKSPACE_STATS_TOOL)
            .count(),
        1,
        "the original MCP tool remains authoritative"
    );
}
