//! Facade turns over the Google provider's streaming driver: a core on the
//! durable backend over a SQLite memory store set serves each turn on its
//! own node, and the host sees each reasoning part once, in its place.

use std::collections::VecDeque;
use std::sync::Arc;

use lash_core::provider::{ProviderHandle, StreamTermination};
use lash_sansio::sync::MutexExt;
use serde_json::{Value, json};

use crate::GoogleOAuthProvider;

/// How long a law waits for a turn that can only hang: a deadlock watchdog,
/// no part of any law.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(120);

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
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RuntimeLookupTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![runtime_lookup_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "lookup").then(|| Arc::new(runtime_lookup_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(json!({ "ok": true })) })
            .await
            .into()
    }
}

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
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
    let provider = GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access"),
    ))
    .with_project_id(Some("test-project".into()))
    .with_stream_termination(StreamTermination::RequireTerminalEvidence)
    .with_transport(transport);
    let core = lash::LashCore::standard_builder(durable_backend().await)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "gemini-test",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("gemini-test")
                            .cache_retention(lash::provider::CacheRetention::Short)
                            .context_window_tokens(16_000)
                            .expose_thinking(true)
                            .build()
                            .expect("valid model spec"),
                        ProviderHandle::new(provider.into_components()),
                    ),
                )
                .expect("register the test model"),
        ))
        .tools(Arc::new(RuntimeLookupTool))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "google-reasoning-boundaries-test",
            "google-reasoning-boundaries-test-boot",
        ))
        .expect("core");
    let session = created_session(&core, "google-reasoning-boundaries").await;

    let output = tokio::time::timeout(
        WATCHDOG,
        session
            .send(lash::TurnInput::text("reason around a tool"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the turn settles")
    .expect("turn");
    let activities = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ReasoningDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let read_view = output.result.state.read_view();
    let durable = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == lash_core::PartKind::Reasoning)
        .map(|part| part.content())
        .collect::<Vec<_>>();

    assert_eq!(activities, ["same reasoning", "same reasoning"]);
    assert_eq!(durable, ["same reasoning", "same reasoning"]);
    assert_eq!(output.assistant_message(), Some("done"));
    core.shutdown().await.expect("the core shuts down");
}

#[tokio::test(flavor = "multi_thread")]
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
    let provider = GoogleOAuthProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new("access"),
    ))
    .with_project_id(Some("test-project".into()))
    .with_stream_termination(StreamTermination::RequireTerminalEvidence)
    .with_transport(transport);
    let core = lash::LashCore::standard_builder(durable_backend().await)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "gemini-test",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("gemini-test")
                            .cache_retention(lash::provider::CacheRetention::Short)
                            .context_window_tokens(16_000)
                            .expose_thinking(true)
                            .build()
                            .expect("valid model spec"),
                        ProviderHandle::new(provider.into_components()),
                    ),
                )
                .expect("register the test model"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "google-signature-only-reasoning-test",
            "google-signature-only-reasoning-test-boot",
        ))
        .expect("core");
    let session = created_session(&core, "google-signature-only-reasoning").await;

    let output = tokio::time::timeout(
        WATCHDOG,
        session
            .send(lash::TurnInput::text("reason about the answer"))
            .output(),
    )
    .await
    .expect("deadlock watchdog: the turn settles")
    .expect("turn");
    let activities = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::ReasoningDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let read_view = output.result.state.read_view();
    let durable = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == lash_core::PartKind::Reasoning)
        .map(|part| part.content())
        .collect::<Vec<_>>();

    assert_eq!(activities, ["visible reasoning"]);
    assert_eq!(durable, ["", "visible reasoning"]);
    core.shutdown().await.expect("the core shuts down");
}

/// The durable backend over a fresh SQLite memory store set: the core
/// built on it serves each session's turn on its own node (ADR 0132).
async fn durable_backend() -> lash::Backend {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds")
}

/// Create the root session `session_id` on the test model.
async fn created_session(core: &lash::LashCore, session_id: &str) -> lash::DurableSession {
    core.session(lash::SessionId::fixture(session_id.to_owned()))
        .create(lash::SessionCreation::root(
            lash_core::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                "gemini-test",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created")
}
