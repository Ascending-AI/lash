//! Facade turns over this crate's providers: a `LashCore` on the durable
//! backend over a SQLite memory store set, whose own node serves each
//! session's turn on the durable path (ADR 0132).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome,
};
use lash::{LashCore, TurnEvent, TurnInput};
use lash_core::provider::{ProviderHandle, ProviderOptions};
use lash_sansio::sync::MutexExt;
use serde_json::{Value, json};

use crate::CodexProvider;
use crate::codex::ws_testing::{ScriptedWsAction, ScriptedWsServer, spawn_scripted_websocket};

/// How long a law waits for a turn that can only hang: a deadlock watchdog,
/// no part of any law.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(120);

/// A core on the durable backend over a fresh SQLite memory store set,
/// serving `model` through `provider`, with `tools` when given.
pub(super) async fn durable_core(
    model: &str,
    metadata: lash::LlmProfileMetadata,
    provider: ProviderHandle,
    tools: Option<Arc<dyn lash::tools::ToolProvider>>,
) -> LashCore {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds");
    let builder = LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new()
                .register(model, lash::RegisteredLlmProfile::new(metadata, provider))
                .expect("register the test model"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate);
    let builder = match tools {
        Some(tools) => builder.tools(tools),
        None => builder,
    };
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            format!("{model}-facade-turns"),
            format!("{model}-facade-turns-boot"),
        ))
        .expect("the core builds")
}

/// Create the root session `session_id` on `model`.
pub(super) async fn session(
    core: &LashCore,
    model: &str,
    session_id: &str,
) -> lash::DurableSession {
    core.session(lash::SessionId::fixture(session_id.to_owned()))
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                model,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created")
}

/// Send `session` the text `input`; the core's node runs the turn and the
/// handle answers its settled output.
pub(super) async fn send(session: &lash::DurableSession, input: &str) -> lash::TurnOutput {
    tokio::time::timeout(WATCHDOG, session.send(TurnInput::text(input)).output())
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("the turn answers")
}

fn websocket_provider(server: &ScriptedWsServer) -> ProviderHandle {
    let provider = CodexProvider::new(std::sync::Arc::new(lash_core::provider::ProviderToken::new("access")))
        .force_websocket_transport()
        // The SSE URL is never dialed on the pinned WebSocket path; point it
        // somewhere unroutable so a regression that falls back to HTTP fails
        // loudly instead of silently passing.
        .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
        .with_options(ProviderOptions {
            reliability: crate::CodexProvider::reliability()
                .request_timeout_ms(Some(5_000))
                .stream_chunk_timeout_ms(Some(2_000)),
            ..ProviderOptions::default()
        });
    ProviderHandle::new(provider.into_components())
}

fn websocket_metadata() -> lash::LlmProfileMetadata {
    lash::LlmProfileMetadata::builder("gpt-5.4")
        .context_window_tokens(16_000)
        .build()
        .expect("valid model spec")
}

/// A facade turn on the Codex WebSocket transport streams the assistant's
/// text as deltas and settles it, over one handshake carrying the session
/// scope and one `response.create` for the configured model.
#[tokio::test(flavor = "multi_thread")]
async fn codex_websocket_facade_turn_streams_text_from_local_server() {
    let server = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_ws_1",
        message_id: "msg_ws_1",
        text: "hello over the websocket",
    }])
    .await;
    let core = durable_core(
        "gpt-5.4",
        websocket_metadata(),
        websocket_provider(&server),
        None,
    )
    .await;
    let session = session(&core, "gpt-5.4", "codex-ws-runtime-text").await;

    let handle = tokio::time::timeout(WATCHDOG, session.send(TurnInput::text("say hello")))
        .await
        .expect("deadlock watchdog: the send is accepted")
        .expect("send");
    let mut stream = handle.events();
    let mut streamed = String::new();
    while let Some(activity) = tokio::time::timeout(WATCHDOG, stream.next_activity())
        .await
        .expect("deadlock watchdog: the turn's activity ends")
    {
        let activity = activity.expect("turn activity");
        if let TurnEvent::AssistantProseDelta { text, .. } = activity.event {
            streamed.push_str(&text);
        }
    }
    let output = tokio::time::timeout(WATCHDOG, handle.output())
        .await
        .expect("deadlock watchdog: the turn settles")
        .expect("turn result");

    assert_eq!(
        output.assistant_message().unwrap_or_default(),
        "hello over the websocket"
    );
    assert_eq!(
        streamed, "hello over the websocket",
        "assistant text must arrive as streamed deltas, not only in the final result"
    );
    let captured = server.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["type"], "response.create");
    assert_eq!(captured[0]["model"], "gpt-5.4");
    assert_eq!(captured[0]["stream"], true);
    let handshakes = server.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert!(
        handshakes[0]
            .iter()
            .any(|(name, value)| name == "session-id" && !value.is_empty()),
        "handshake must carry the runtime session scope, got {handshakes:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}

struct EchoProbe {
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl StaticToolExecute for EchoProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "echo_probe");
            self.seen.lock_recover().push(call.args.clone());
            ToolOutcome::ok(json!({ "echo": call.args }))
        })
        .await
        .into()
    }
}

fn echo_probe_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:echo_probe",
        "echo_probe",
        "Echo the provided arguments.",
        json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A tool call streamed over the Codex WebSocket runs once, and the
/// follow-up request carries its output back as `function_call_output`.
#[tokio::test(flavor = "multi_thread")]
async fn codex_websocket_facade_turn_round_trips_a_tool_call() {
    let server = spawn_scripted_websocket(vec![
        ScriptedWsAction::ToolCall {
            response_id: "resp_ws_tool",
            call_id: "call_echo_1",
            tool_name: "echo_probe",
            arguments: r#"{"value":"ping"}"#,
        },
        ScriptedWsAction::Complete {
            response_id: "resp_ws_2",
            message_id: "msg_ws_2",
            text: "tool round trip complete",
        },
    ])
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = durable_core(
        "gpt-5.4",
        websocket_metadata(),
        websocket_provider(&server),
        Some(Arc::new(StaticToolProvider::new(
            vec![echo_probe_definition()],
            EchoProbe {
                seen: Arc::clone(&seen),
            },
        ))),
    )
    .await;
    let session = session(&core, "gpt-5.4", "codex-ws-runtime-tool").await;

    let output = send(&session, "probe the echo tool").await;

    assert_eq!(
        output.assistant_message().unwrap_or_default(),
        "tool round trip complete"
    );
    assert_eq!(
        seen.lock_recover().as_slice(),
        [json!({"value": "ping"})],
        "the runtime must execute the tool call streamed over the websocket"
    );
    let captured = server.captured();
    assert_eq!(captured.len(), 2, "expected two response.create requests");
    assert!(captured.iter().all(|req| req["type"] == "response.create"));
    let follow_up_input = captured[1]["input"].as_array().expect("input items");
    assert!(
        follow_up_input.iter().any(|item| {
            item["type"] == "function_call_output" && item["call_id"] == "call_echo_1"
        }),
        "follow-up request must echo the tool output as function_call_output, got {follow_up_input:?}"
    );
    core.shutdown().await.expect("the core shuts down");
}
