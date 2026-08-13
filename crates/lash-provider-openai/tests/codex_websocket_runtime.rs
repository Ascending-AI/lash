//! Runtime-level Codex WebSocket tests: full facade turns (`LashCore` +
//! `ProviderHandle`) executed end-to-end over the provider's WebSocket
//! transport against the local scripted server from
//! [`lash_provider_openai::codex::ws_testing`].
//!
//! The endpoint override rides the constructor-level seam
//! (`CodexProvider::with_endpoint_urls` + `force_websocket_transport`), the
//! same injection philosophy as `with_http_transport`: nothing here touches
//! env vars or serialized provider config.

use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolResult};
use lash::{LashCore, TurnEvent, TurnInput};
use lash_core::provider::{ProviderHandle, ProviderOptions, ProviderReliability, RequestTimeout};
use lash_provider_openai::CodexProvider;
use lash_provider_openai::codex::ws_testing::{
    ScriptedWsAction, ScriptedWsServer, spawn_scripted_websocket,
};
use serde_json::{Value, json};

fn websocket_provider(server: &ScriptedWsServer) -> ProviderHandle {
    let provider = CodexProvider::new("access", "refresh", 0)
        .force_websocket_transport()
        // The SSE URL is never dialed on the pinned WebSocket path; point it
        // somewhere unroutable so a regression that falls back to HTTP fails
        // loudly instead of silently passing.
        .with_endpoint_urls("http://127.0.0.1:9/unused-sse", server.url.clone())
        .with_options(ProviderOptions {
            reliability: ProviderReliability::codex()
                .request_timeout(Some(RequestTimeout::Millis(5_000)))
                .stream_chunk_timeout_ms(Some(2_000)),
            ..ProviderOptions::default()
        });
    ProviderHandle::new(provider.into_components())
}

fn websocket_core(provider: ProviderHandle) -> LashCore {
    LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("gpt-5.4")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "codex-websocket-runtime-test",
            "codex-websocket-runtime-test-boot",
        ))
        .expect("core")
}

#[tokio::test]
async fn codex_websocket_facade_turn_streams_text_from_local_server() {
    let server = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "resp_ws_1",
        message_id: "msg_ws_1",
        text: "hello over the websocket",
    }])
    .await;
    let core = websocket_core(websocket_provider(&server));
    let session = core
        .session("codex-ws-runtime-text")
        .open()
        .await
        .expect("session");

    let mut stream = session
        .turn(TurnInput::text("say hello"))
        .stream()
        .expect("turn stream");
    let mut streamed = String::new();
    while let Some(activity) = stream.next_activity().await {
        let activity = activity.expect("turn activity");
        if let TurnEvent::AssistantProseDelta { text } = activity.event {
            streamed.push_str(&text);
        }
    }
    let result = stream.finish().await.expect("turn result");

    assert_eq!(
        result.assistant_message().unwrap_or_default(),
        "hello over the websocket"
    );
    assert_eq!(
        streamed, "hello over the websocket",
        "assistant text must arrive as streamed deltas, not only in the final result"
    );

    // The turn rode the WebSocket transport: the scripted server saw exactly
    // one `response.create` for the configured model, over one handshake that
    // carried the runtime-supplied session scope.
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
}

struct EchoProbe {
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl StaticToolExecute for EchoProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        assert_eq!(call.name, "echo_probe");
        self.seen.lock_recover().push(call.args.clone());
        ToolResult::ok(json!({ "echo": call.args }))
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
}

#[tokio::test]
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
    let core = LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(websocket_provider(&server))
        .model(
            lash::ModelSpec::builder("gpt-5.4")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .tools(Arc::new(StaticToolProvider::new(
            vec![echo_probe_definition()],
            EchoProbe {
                seen: Arc::clone(&seen),
            },
        )))
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "codex-websocket-runtime-tool-test",
            "codex-websocket-runtime-tool-test-boot",
        ))
        .expect("core");
    let session = core
        .session("codex-ws-runtime-tool")
        .open()
        .await
        .expect("session");

    let output = session
        .turn(TurnInput::text("probe the echo tool"))
        .run()
        .await
        .expect("turn");

    assert_eq!(
        output.assistant_message().unwrap_or_default(),
        "tool round trip complete"
    );
    assert_eq!(
        seen.lock_recover().as_slice(),
        [json!({"value": "ping"})],
        "the runtime must execute the tool call streamed over the websocket"
    );

    // Two websocket requests: the tool-call turn iteration and the follow-up
    // that carries the tool output back.
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
}
