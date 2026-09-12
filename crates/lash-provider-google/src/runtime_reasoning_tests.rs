use std::collections::VecDeque;
use std::sync::Arc;

use lash_core::provider::{ProviderHandle, ProviderOptions, StreamTermination};
use lash_sansio::sync::MutexExt;
use serde_json::{Value, json};

use crate::{GoogleOAuthClient, GoogleOAuthProvider};

#[derive(Debug)]
struct ScriptedSseTransport {
    bodies: std::sync::Mutex<VecDeque<String>>,
}

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for ScriptedSseTransport {
    async fn send(
        &self,
        _request: lash_llm_transport::LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
    {
        let body = self
            .bodies
            .lock_recover()
            .pop_front()
            .expect("scripted Google response");
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: lash_llm_transport::LlmHttpBody::buffered(body),
        })
    }
}

struct RuntimeLookupTool;

fn runtime_lookup_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:lookup",
        "lookup",
        "Look up a value.",
        json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"],
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RuntimeLookupTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![runtime_lookup_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "lookup").then(|| Arc::new(runtime_lookup_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        lash_core::ToolOutcome::ok(json!({ "ok": true }))
    }
}

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[tokio::test]
async fn google_streaming_runtime_preserves_tool_interleaved_reasoning_boundaries() {
    let first = vec![
        json!({"response":{"candidates":[{"content":{"parts":[{
            "text":"same reasoning",
            "thought":true
        }]}}]}}),
        json!({"response":{"candidates":[{"content":{"parts":[{
            "functionCall":{"id":"call-1","name":"lookup","args":{"q":"x"}}
        }]}}]}}),
        json!({"response":{"candidates":[{
            "content":{"parts":[{
                "text":"same reasoning",
                "thought":true
            }]},
            "finishReason":"STOP"
        }]}}),
    ];
    let second = vec![json!({"response":{"candidates":[{
        "content":{"parts":[{"text":"done"}]},
        "finishReason":"STOP"
    }]}})];
    let transport = Arc::new(ScriptedSseTransport {
        bodies: std::sync::Mutex::new([sse_body(&first), sse_body(&second)].into_iter().collect()),
    });
    let provider = GoogleOAuthProvider::new(
        "access",
        "refresh",
        u64::MAX,
        GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("test-project".into()))
    .with_options(ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    })
    .with_stream_termination(StreamTermination::RequireTerminalEvidence)
    .with_transport(transport);
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .provider(ProviderHandle::new(provider.into_components()))
        .model(
            lash::ModelSpec::builder("gemini-test")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .tools(Arc::new(RuntimeLookupTool))
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "google-reasoning-boundaries-test",
            "google-reasoning-boundaries-test-boot",
        ))
        .expect("core");
    let session = core
        .session("google-reasoning-boundaries")
        .open()
        .await
        .expect("session");

    let output = session
        .turn(lash::TurnInput::text("reason around a tool"))
        .run()
        .await
        .expect("turn");
    let activities = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ReasoningDelta { text } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let read_view = output
        .result
        .state
        .read_view()
        .expect("test runtime frame scope resolves");
    let durable = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind == lash_core::PartKind::Reasoning)
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>();

    assert_eq!(activities, ["same reasoning", "same reasoning"]);
    assert_eq!(durable, ["same reasoning", "same reasoning"]);
    assert_eq!(output.assistant_message(), Some("done"));
}

#[tokio::test]
async fn google_streaming_runtime_does_not_republish_reasoning_after_signature_only_part() {
    let response = vec![json!({"response":{"candidates":[{
        "content":{"parts":[
            {
                "thought":true,
                "thoughtSignature":"U0lHLTE="
            },
            {
                "text":"visible reasoning",
                "thought":true,
                "thoughtSignature":"U0lHLTI="
            }
        ]},
        "finishReason":"STOP"
    }]}})];
    let transport = Arc::new(ScriptedSseTransport {
        bodies: std::sync::Mutex::new([sse_body(&response)].into_iter().collect()),
    });
    let provider = GoogleOAuthProvider::new(
        "access",
        "refresh",
        u64::MAX,
        GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("test-project".into()))
    .with_options(ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    })
    .with_stream_termination(StreamTermination::RequireTerminalEvidence)
    .with_transport(transport);
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .provider(ProviderHandle::new(provider.into_components()))
        .model(
            lash::ModelSpec::builder("gemini-test")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "google-signature-only-reasoning-test",
            "google-signature-only-reasoning-test-boot",
        ))
        .expect("core");
    let session = core
        .session("google-signature-only-reasoning")
        .open()
        .await
        .expect("session");

    let output = session
        .turn(lash::TurnInput::text("reason about the answer"))
        .run()
        .await
        .expect("turn");
    let activities = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ReasoningDelta { text } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let read_view = output
        .result
        .state
        .read_view()
        .expect("test runtime frame scope resolves");
    let durable = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind == lash_core::PartKind::Reasoning)
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>();

    assert_eq!(activities, ["visible reasoning"]);
    assert_eq!(durable, ["", "visible reasoning"]);
}
