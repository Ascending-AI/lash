use lash::sync::MutexExt;
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
use lash::tools::{
    ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider,
};
use lash::{ModelSpec, TurnInput};
use lash_plugin_mcp::{McpServerConfig, TimeoutDisconnectPolicy};
use serde_json::{Value, json};
use slack_clone::bot::mcp_admin;
use slack_clone::bot::runtime::{self, BotRuntime, RuntimeConfig};
use slack_clone::bot::slack_api::SlackApi;
use slack_clone::mcp_server::{
    API_BASE_URL_ENV, BOT_TOKEN_ENV, ELICIT_CONFIRMATION_TOOL, LIST_CHANNELS_SUMMARY_TOOL,
    LIST_HOST_ROOTS_TOOL, SAMPLE_SUMMARY_TOOL, URL_ELICITATION_TOOL, WORKSPACE_STATS_TOOL,
};
use slack_clone::{mcp_http_server, mcp_server};
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
    ToolWithInput(&'static str, &'static str),
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
                        .lock_recover()
                        .push(serde_json::to_string(&request).expect("serialize request"));
                    let step = steps.lock().await.pop_front().unwrap_or(Step::Text("done"));
                    let response = match step {
                        Step::Tool(name) => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "mcp-call".to_string(),
                                tool_name: name.to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        },
                        Step::ToolWithInput(name, input_json) => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "mcp-call".to_string(),
                                tool_name: name.to_string(),
                                input_json: input_json.to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        },
                        Step::Text(text) => {
                            let response = LlmResponse {
                                parts: vec![LlmOutputPart::Text {
                                    text: text.to_string(),
                                    response_meta: None,
                                }],
                                ..LlmResponse::default()
                            };
                            assert!(
                                LlmResponse::full_text(&response).contains(text),
                                "projected response must contain the scripted text"
                            );
                            response
                        }
                    };
                    Ok(response)
                }
            })
            .build()
            .into_handle()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock_recover().clone()
    }
}

#[tokio::test]
async fn bundled_server_exercises_sampling_both_elicitation_modes_and_roots_through_host_seams() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let state = FakeApiState::normal();
    let (api_base_url, _server) = fake_api(state).await;
    let script = Script::new([
        Step::ToolWithInput(
            SAMPLE_SUMMARY_TOOL,
            r#"{"text":"Lash keeps MCP policy with the embedding host."}"#,
        ),
        Step::Text("Host-generated summary."),
        Step::Tool(ELICIT_CONFIRMATION_TOOL),
        Step::Tool(URL_ELICITATION_TOOL),
        Step::Tool(LIST_HOST_ROOTS_TOOL),
        Step::Text("All MCP client features completed."),
    ]);
    let core = build_core(
        scratch.path(),
        &api_base_url,
        &script,
        direct_server_config(&api_base_url),
    )
    .await;
    let session = core
        .session("mcp-client-depth")
        .open()
        .await
        .expect("open session");

    let turn = session
        .turn(TurnInput::text("@lashbot exercise MCP client depth"))
        .run()
        .await
        .expect("run MCP client-depth turn");
    assert_eq!(turn.result.tool_calls.len(), 4);
    assert_eq!(
        turn.result
            .tool_calls
            .iter()
            .map(|call| call.tool.as_str())
            .collect::<Vec<_>>(),
        vec![
            SAMPLE_SUMMARY_TOOL,
            ELICIT_CONFIRMATION_TOOL,
            URL_ELICITATION_TOOL,
            LIST_HOST_ROOTS_TOOL
        ]
    );

    let requests = script.requests();
    assert_eq!(requests.len(), 6, "main loop plus one nested host sample");
    assert!(
        requests[1].contains("Summarize this in one short sentence")
            && requests[1].contains("Lash keeps MCP policy"),
        "the MCP server's sampling prompt reaches the host model: {}",
        requests[1]
    );
    assert!(
        requests[2].contains("Host-generated summary.") && requests[2].contains("mock/model"),
        "the sampled result returns to the outer tool attempt: {}",
        requests[2]
    );
    assert!(
        requests[3].contains("\\\"action\\\":\\\"accept\\\"")
            && requests[3].contains("\\\"answer\\\":\\\"yes\\\""),
        "the host's structured elicitation answer returns to the server: {}",
        requests[3]
    );
    assert!(
        requests[4].contains("slack-clone-demo-url-1")
            && requests[4].contains("\\\"completion_notified\\\":true"),
        "the URL request is accepted and its completion is notified: {}",
        requests[4]
    );
    assert!(
        requests[5].contains("file://") && requests[5].contains("slack-clone"),
        "the host-supplied workspace root returns to the server: {}",
        requests[5]
    );

    slack_clone::bot::shutdown_core(&core)
        .await
        .expect("shut down bot core");
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
        call_policy: lash_plugin_mcp::McpCallPolicy {
            call_timeout_ms: 5_000,
            ..Default::default()
        },
        shutdown_policy: Default::default(),
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
        call_policy: lash_plugin_mcp::McpCallPolicy {
            call_timeout_ms: 5_000,
            ..Default::default()
        },
        shutdown_policy: Default::default(),
        binary_content_attachments: false,
    }
}

async fn build_core(
    root: &std::path::Path,
    api_base_url: &str,
    script: &Script,
    server: McpServerConfig,
) -> lash::LashCore {
    build_runtime(root, api_base_url, script, Some(server))
        .await
        .core
}

async fn build_runtime(
    root: &std::path::Path,
    api_base_url: &str,
    script: &Script,
    server: Option<McpServerConfig>,
) -> BotRuntime {
    let mut config = RuntimeConfig::new(root);
    config.trace_to_stderr = false;
    if let Some(server) = server {
        config
            .mcp_servers
            .insert(mcp_server::SERVER_NAME.to_string(), server);
    }
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

#[cfg(unix)]
#[tokio::test]
async fn bot_exit_path_explicitly_stops_the_mcp_child() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let state = FakeApiState::normal();
    let (api_base_url, _server) = fake_api(state).await;
    let pid_file = scratch.path().join("shutdown-mcp.pid");
    let script = Script::new([Step::Text("unused")]);
    let core = build_core(
        &scratch.path().join("lash"),
        &api_base_url,
        &script,
        wrapped_server_config(&api_base_url, &pid_file),
    )
    .await;
    let pid = read_pid(&pid_file).await;
    assert!(
        process_exists(pid),
        "MCP child must be live before bot exit"
    );

    slack_clone::bot::shutdown_core(&core)
        .await
        .expect("shut down bot core");

    assert!(
        !process_exists(pid),
        "the bot exit path must reap the MCP child through LashCore::shutdown"
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

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("probe process")
        .success()
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(json!({ "wrong": true }))
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

// ---------------------------------------------------------------------------
// The bundled streamable-HTTP server: the other transport lash supports, the
// runtime attach/detach lifecycle, and the three host policies that only this
// transport's configuration can express.
// ---------------------------------------------------------------------------

/// Serve the bundled HTTP MCP server on an ephemeral loopback port.
async fn http_mcp_server(token: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP MCP server");
    let addr = listener.local_addr().expect("HTTP MCP server address");
    let router = mcp_http_server::router(token.to_string());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve HTTP MCP");
    });
    (
        format!("http://{addr}{}", mcp_http_server::MCP_PATH),
        server,
    )
}

async fn catalog_names(core: &lash::LashCore, session_id: &str) -> Vec<String> {
    core.session(session_id)
        .open()
        .await
        .expect("open session")
        .tools()
        .active_manifests()
        .await
        .expect("read catalog")
        .into_iter()
        .map(|manifest| manifest.name)
        .collect()
}

fn status_of(runtime: &BotRuntime, server_name: &str) -> lash_plugin_mcp::McpServerStatus {
    runtime
        .mcp
        .server_statuses()
        .into_iter()
        .find(|status| status.server_name == server_name)
        .unwrap_or_else(|| panic!("no MCP status for `{server_name}`"))
}

#[tokio::test]
async fn the_configured_http_header_is_what_lets_the_transport_connect() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([Step::Text("unused")]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;

    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");
    let accepted = status_of(&runtime, mcp_http_server::SERVER_NAME);
    assert!(accepted.connected, "last_error: {:?}", accepted.last_error);
    assert_eq!(accepted.tool_count, 5);
    assert_eq!(accepted.last_error, None);

    // Same server, same URL, wrong credential: attach still succeeds, because a
    // server that is refusing right now is a server the pool keeps retrying.
    // The evidence that the header did anything is the status row.
    runtime
        .mcp
        .attach_server(
            "workspace_http_denied".to_string(),
            runtime::http_mcp_server_config(&url, "wrong-token"),
        )
        .await
        .expect("attach registers a server the credential is rejected by");
    let refused = status_of(&runtime, "workspace_http_denied");
    assert!(!refused.connected);
    assert_eq!(refused.tool_count, 0);
    let last_error = refused.last_error.unwrap_or_default();
    assert!(
        last_error.contains("401") || last_error.to_lowercase().contains("unauthorized"),
        "the rejection must name the credential failure: {last_error}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn attaching_and_detaching_an_http_server_moves_its_tools_through_the_catalog() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::ROOTS_CHANGE_REPORT_TOOL),
        Step::Text("The HTTP integration answered."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;

    let before = catalog_names(&runtime.core, "mcp-http-before").await;
    assert!(
        !before
            .iter()
            .any(|name| name.starts_with("mcp__workspace_http__")),
        "no HTTP MCP tools before the attach: {before:?}"
    );

    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");

    let attached = catalog_names(&runtime.core, "mcp-http-attached").await;
    for tool in [
        mcp_http_server::WORKSPACE_BADGE_TOOL,
        mcp_http_server::ROOTS_CHANGE_REPORT_TOOL,
        mcp_http_server::ELICIT_PICK_COUNT_TOOL,
        mcp_http_server::STALL_TOOL,
    ] {
        assert!(attached.iter().any(|name| name == tool), "missing {tool}");
    }

    // A session opened after the attach can actually route to the new server,
    // not merely list it.
    let session = runtime
        .core
        .session("mcp-http-call")
        .open()
        .await
        .expect("open session");
    let turn = session
        .turn(TurnInput::text("@lashbot check the HTTP integration"))
        .run()
        .await
        .expect("run a turn against the attached server");
    assert_eq!(turn.result.tool_calls.len(), 1);
    let output = turn.result.tool_calls[0].output.value_for_projection();
    assert_eq!(
        output["notifications_seen"], 0,
        "no roots change has been published yet: {output}"
    );

    runtime
        .mcp
        .detach_server(mcp_http_server::SERVER_NAME)
        .await
        .expect("detach the HTTP MCP server");
    let after = catalog_names(&runtime.core, "mcp-http-after").await;
    assert!(
        !after
            .iter()
            .any(|name| name.starts_with("mcp__workspace_http__")),
        "the detached server's tools must leave the catalog: {after:?}"
    );
    assert!(
        runtime
            .mcp
            .server_statuses()
            .iter()
            .all(|status| status.server_name != mcp_http_server::SERVER_NAME),
        "a detached server must not remain in the pool's status list"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

/// Every file the host's attachment store holds, as raw bytes.
fn stored_attachment_bytes(root: &std::path::Path) -> Vec<Vec<u8>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                found.push(bytes);
            }
        }
    }
    found
}

#[tokio::test]
async fn binary_mcp_content_becomes_an_attachment_only_where_the_host_opted_in() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::WORKSPACE_BADGE_TOOL),
        Step::Text("Badge stored."),
        Step::Tool("mcp__workspace_inline__workspace_badge"),
        Step::Text("Badge inline."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;

    // Same server, same tool, two host policies.
    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the attachment-persisting server");
    runtime
        .mcp
        .attach_server(
            "workspace_inline".to_string(),
            runtime::http_mcp_server_config(&url, "integration-token")
                .with_binary_content_attachments(false),
        )
        .await
        .expect("attach the inline server");

    let session = runtime
        .core
        .session("mcp-http-badge")
        .open()
        .await
        .expect("open session");
    let stored = session
        .turn(TurnInput::text("@lashbot fetch the badge"))
        .run()
        .await
        .expect("run the opted-in badge turn");
    let process_hopped_output = lash::process::ProcessAwaitOutput::from_tool_output(
        stored.result.tool_calls[0].output.clone(),
    )
    .into_tool_output();
    assert_eq!(
        process_hopped_output.attachments().len(),
        1,
        "the MCP image attachment must survive the background-process hop to the model"
    );
    let stored_output = stored.result.tool_calls[0].output.value_for_projection();
    let attachment = stored_output
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["$lash_tool_value"] == "attachment")
        })
        .unwrap_or_else(|| panic!("no attachment in the opted-in result: {stored_output}"));
    assert_eq!(attachment["source"]["source"], "stored");
    assert_eq!(
        attachment["source"]["attachment_ref"]["media_type"],
        mcp_http_server::BADGE_MEDIA_TYPE
    );
    assert_eq!(
        attachment["source"]["attachment_ref"]["byte_len"],
        mcp_http_server::BADGE_BYTES.len()
    );
    let files = stored_attachment_bytes(&scratch.path().join("attachments"));
    assert!(
        files
            .iter()
            .any(|bytes| bytes == mcp_http_server::BADGE_BYTES),
        "the host's attachment store must hold the server's exact bytes"
    );

    let inline = session
        .turn(TurnInput::text("@lashbot fetch the badge again"))
        .run()
        .await
        .expect("run the opted-out badge turn");
    let inline_output = inline.result.tool_calls[0].output.value_for_projection();
    let encoded = inline_output.to_string();
    assert!(
        !encoded.contains("\"$lash_tool_value\":\"attachment\""),
        "an opted-out server's binary content must stay inline: {inline_output}"
    );
    assert!(
        encoded.contains(mcp_http_server::BADGE_URI),
        "the inline result must carry the resource itself: {inline_output}"
    );
    assert_eq!(
        stored_attachment_bytes(&scratch.path().join("attachments")).len(),
        files.len(),
        "the opted-out call must not write to the attachment store"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn a_stalled_call_times_out_as_a_tool_failure_and_keeps_the_connection() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::STALL_TOOL),
        Step::Text("The stalled call failed cleanly."),
        Step::Tool(mcp_http_server::ROOTS_CHANGE_REPORT_TOOL),
        Step::Text("The connection survived."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");

    let session = runtime
        .core
        .session("mcp-http-stall")
        .open()
        .await
        .expect("open session");
    let stalled = session
        .turn(TurnInput::text("@lashbot call the stalling tool"))
        .run()
        .await
        .expect("the call timeout is model-visible, not a turn failure");
    let failure = stalled.result.tool_calls[0].output.value_for_projection();
    assert_eq!(failure["class"], "timeout");
    assert_eq!(failure["code"], "mcp_call_timeout");

    // The host keeps the default disconnect policy, so the timeout triggers a
    // liveness probe. This server is alive and answers it, which is exactly the
    // case the default is for: a slow tool is reported as a typed timeout and
    // the connection is kept.
    let status = status_of(&runtime, mcp_http_server::SERVER_NAME);
    assert!(status.connected, "last_error: {:?}", status.last_error);
    session
        .turn(TurnInput::text("@lashbot check the integration again"))
        .run()
        .await
        .expect("the next call reuses the same connection");
    let requests = script.requests();
    assert!(
        requests[3].contains("notifications_seen"),
        "the surviving connection answers the next call: {}",
        requests[3]
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

/// The opt-out a host reaches for when a timeout must never cost the peer.
///
/// `TimeoutDisconnectPolicy::Never` is deliberately *not* what
/// [`runtime::http_mcp_server_config`] ships: with the default liveness probe
/// interval of `0`, nothing else ever tests the peer, so a server that died
/// mid-call would keep reporting `connected: true`. A host that sets it is
/// buying "one slow tool never costs the connection" with "a dead peer looks
/// healthy", and this test is where that trade is written down.
#[tokio::test]
async fn a_host_can_opt_out_of_timeout_disconnects_entirely() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::STALL_TOOL),
        Step::Text("The stalled call failed cleanly."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    let mut config = runtime::http_mcp_server_config(&url, "integration-token");
    // There is no builder for this field yet; the transport variant is public,
    // so a host that wants the opt-out pokes the flattened call policy.
    let McpServerConfig::StreamableHttp { call_policy, .. } = &mut config else {
        unreachable!("streamable_http returns the HTTP transport")
    };
    call_policy.timeout_disconnect_policy = TimeoutDisconnectPolicy::Never;
    assert_eq!(
        call_policy.timeout_disconnect_policy,
        TimeoutDisconnectPolicy::Never
    );
    runtime
        .mcp
        .attach_server(mcp_http_server::SERVER_NAME.to_string(), config)
        .await
        .expect("attach the HTTP MCP server");

    let session = runtime
        .core
        .session("mcp-http-never-disconnect")
        .open()
        .await
        .expect("open session");
    let stalled = session
        .turn(TurnInput::text("@lashbot call the stalling tool"))
        .run()
        .await
        .expect("the call timeout is model-visible, not a turn failure");
    let failure = stalled.result.tool_calls[0].output.value_for_projection();
    assert_eq!(failure["class"], "timeout");
    assert_eq!(failure["code"], "mcp_call_timeout");
    let status = status_of(&runtime, mcp_http_server::SERVER_NAME);
    assert!(
        status.connected,
        "no probe runs under `Never`, so the entry stays connected: {:?}",
        status.last_error
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn a_form_the_answer_book_cannot_satisfy_is_declined_rather_than_answered() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::ELICIT_PICK_COUNT_TOOL),
        Step::Text("The host declined."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");

    let session = runtime
        .core
        .session("mcp-http-elicit")
        .open()
        .await
        .expect("open session");
    let turn = session
        .turn(TurnInput::text("@lashbot ask how many badges"))
        .run()
        .await
        .expect("run the elicitation turn");
    let output = turn.result.tool_calls[0].output.value_for_projection();
    assert_eq!(
        output["action"], "decline",
        "the host's textual answer fails the server's integer schema, so it must \
         decline instead of sending it: {output}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

/// Standing consent is per question, not per field name.
#[tokio::test]
async fn a_question_the_host_has_not_read_is_declined_even_with_a_familiar_field() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::ELICIT_UNKNOWN_PROMPT_TOOL),
        Step::Text("The host declined."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");

    let session = runtime
        .core
        .session("mcp-http-unknown-prompt")
        .open()
        .await
        .expect("open session");
    let turn = session
        .turn(TurnInput::text("@lashbot ask the unread question"))
        .run()
        .await
        .expect("run the elicitation turn");
    let output = turn.result.tool_calls[0].output.value_for_projection();
    // The field is `answer`, which the host answers "yes" to for the prompt it
    // has read. A different question with the same field must still be
    // declined, or the host is granting consent it was never asked for.
    assert_eq!(
        output["action"], "decline",
        "a trusted server asking an unread question must still be declined: {output}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

/// The operator credential is not the platform's event authenticator.
///
/// The verification token is embedded by the platform in every event envelope
/// it delivers, so it says who is calling, not what they may do. Attaching a
/// tool source is a privileged act and gets its own credential.
#[test]
fn the_admin_credential_is_configured_separately_from_the_verification_token() {
    let config = slack_clone::bot::BotConfig::from_env().expect("read bot config defaults");
    assert_ne!(
        config.admin_token, config.verification_token,
        "the MCP admin API must not be unlocked by the token the platform already holds"
    );
}

#[tokio::test]
async fn publishing_a_root_notifies_the_connected_server_which_re_reads_the_list() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::ROOTS_CHANGE_REPORT_TOOL),
        Step::Text("Roots reported."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    runtime
        .mcp
        .attach_server(
            mcp_http_server::SERVER_NAME.to_string(),
            runtime::http_mcp_server_config(&url, "integration-token"),
        )
        .await
        .expect("attach the HTTP MCP server");

    let published = runtime
        .roots
        .publish(
            "file:///srv/slack-clone/exports".to_string(),
            Some("exports".to_string()),
        )
        .await;
    assert_eq!(published, 2, "the workspace root plus the published one");
    runtime
        .mcp
        .notify_roots_changed()
        .await
        .expect("notify connected servers");

    let session = runtime
        .core
        .session("mcp-http-roots")
        .open()
        .await
        .expect("open session");
    let turn = session
        .turn(TurnInput::text("@lashbot report the roots"))
        .run()
        .await
        .expect("run the roots-report turn");
    let output = turn.result.tool_calls[0].output.value_for_projection();
    assert_eq!(
        output["notifications_seen"], 1,
        "the server must have received exactly one roots-changed notification: {output}"
    );
    let roots = output["roots"].as_array().cloned().unwrap_or_default();
    assert!(
        roots.iter().any(|root| root == "exports"),
        "the re-read list must contain the newly published root: {output}"
    );
    assert!(
        roots.iter().any(|root| root == "slack-clone"),
        "the original workspace root must survive publication: {output}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

// ---------------------------------------------------------------------------
// The operator surface. Attaching an integration is an operator action on the
// bot's own HTTP API, never a tool the model can reach.
// ---------------------------------------------------------------------------

const ADMIN_TOKEN: &str = "slack-clone-admin-test-token";

async fn serve_admin(runtime: &BotRuntime) -> String {
    let admin = mcp_admin::McpAdmin::new(runtime, ADMIN_TOKEN);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind admin API");
    let addr = listener.local_addr().expect("admin API address");
    tokio::spawn(async move {
        axum::serve(listener, mcp_admin::router(admin))
            .await
            .expect("serve admin API");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_operator_api_attaches_lists_and_detaches_an_integration() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([Step::Text("unused")]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    let admin_url = serve_admin(&runtime).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .send()
        .await
        .expect("send unauthenticated request");
    assert_eq!(
        unauthorized.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the MCP admin API must not be reachable without the bot's shared secret"
    );

    let attached: Value = client
        .post(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({
            "name": mcp_http_server::SERVER_NAME,
            "url": url,
            "token": "integration-token",
        }))
        .send()
        .await
        .expect("attach through the admin API")
        .json()
        .await
        .expect("attach response body");
    assert_eq!(attached["connected"], true);
    assert_eq!(attached["tool_count"], 5);

    let listed: Value = client
        .get(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("list servers")
        .json()
        .await
        .expect("list response body");
    let tools = listed["servers"][0]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        tools
            .iter()
            .any(|tool| tool == mcp_http_server::WORKSPACE_BADGE_TOOL),
        "an operator view has to name the tools the integration advertises: {listed}"
    );

    let detached = client
        .delete(format!(
            "{admin_url}/admin/mcp/servers/{}",
            mcp_http_server::SERVER_NAME
        ))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("detach through the admin API");
    assert_eq!(detached.status(), reqwest::StatusCode::OK);
    let listed: Value = client
        .get(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("list servers after detach")
        .json()
        .await
        .expect("list response body");
    assert_eq!(
        listed["servers"].as_array().map(Vec::len),
        Some(0),
        "the detached integration must leave the operator view: {listed}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn the_operator_api_lists_tools_for_a_non_normalized_server_name() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([Step::Text("unused")]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    let admin_url = serve_admin(&runtime).await;
    let client = reqwest::Client::new();

    let attached: Value = client
        .post(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({
            "name": "My Server",
            "url": url,
            "token": "integration-token",
        }))
        .send()
        .await
        .expect("attach server with a non-normalized configured name")
        .json()
        .await
        .expect("attach response body");
    assert_eq!(attached["tool_count"], 5);
    assert_eq!(
        attached["tools"].as_array().map(Vec::len),
        Some(5),
        "operator view must join tools through the configured server identity: {attached}"
    );

    let listed: Value = client
        .get(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("list servers")
        .json()
        .await
        .expect("list response body");
    assert_eq!(listed["servers"][0]["name"], "My Server");
    assert_eq!(
        listed["servers"][0]["tools"].as_array().map(Vec::len),
        Some(5),
        "listed server must retain the same explicit tool relation: {listed}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn an_invalid_server_name_is_the_operators_error_not_a_gateway_failure() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([Step::Text("unused")]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    let admin_url = serve_admin(&runtime).await;

    // `__` is the tool-name separator, so it cannot appear in a server name.
    let rejected = reqwest::Client::new()
        .post(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "name": "bad__name", "url": url, "token": "integration-token" }))
        .send()
        .await
        .expect("attach an invalid server name");
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = rejected.json().await.expect("rejection body");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("__"),
        "the typed configuration error must reach the operator: {body}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}

#[tokio::test]
async fn publishing_a_root_through_the_operator_api_reaches_the_connected_server() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let (api_base_url, _api) = fake_api(FakeApiState::normal()).await;
    let (url, _server) = http_mcp_server("integration-token").await;
    let script = Script::new([
        Step::Tool(mcp_http_server::ROOTS_CHANGE_REPORT_TOOL),
        Step::Text("Roots reported."),
    ]);
    let runtime = build_runtime(scratch.path(), &api_base_url, &script, None).await;
    let admin_url = serve_admin(&runtime).await;
    let client = reqwest::Client::new();
    client
        .post(format!("{admin_url}{}", mcp_admin::SERVERS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({
            "name": mcp_http_server::SERVER_NAME,
            "url": url,
            "token": "integration-token",
        }))
        .send()
        .await
        .expect("attach through the admin API");

    let published: Value = client
        .post(format!("{admin_url}{}", mcp_admin::ROOTS_PATH))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "uri": "file:///srv/slack-clone/exports", "name": "exports" }))
        .send()
        .await
        .expect("publish a root")
        .json()
        .await
        .expect("publish response body");
    assert_eq!(published["roots"], 2);
    assert_eq!(published["notified"], true);

    let session = runtime
        .core
        .session("mcp-admin-roots")
        .open()
        .await
        .expect("open session");
    let turn = session
        .turn(TurnInput::text("@lashbot report the roots"))
        .run()
        .await
        .expect("run the roots-report turn");
    let output = turn.result.tool_calls[0].output.value_for_projection();
    assert_eq!(output["notifications_seen"], 1);
    assert!(
        output["roots"]
            .as_array()
            .is_some_and(|roots| roots.iter().any(|root| root == "exports")),
        "the operator's published root must reach the server: {output}"
    );

    slack_clone::bot::shutdown_core(&runtime.core)
        .await
        .expect("shut down bot core");
}
