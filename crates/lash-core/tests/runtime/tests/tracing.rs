// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use ::tracing::Instrument;
use lash_core::facade_support::ToolStateFacadeOps;
use lash_sansio::sync::MutexExt;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

const SEED: u64 = 0x5_f50a;

/// A protocol session whose rendered system prompt is the text it holds.
struct SwitchablePrompt(Arc<std::sync::Mutex<&'static str>>);

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for SwitchablePrompt {
    async fn render_system_prompt(
        &self,
        _ctx: lash_core::plugin::SystemPromptContext<'_>,
    ) -> Result<Arc<str>, lash_core::SessionError> {
        Ok(Arc::from(*self.0.lock().expect("prompt lock")))
    }
}

struct SchemaChangingTool {
    revision: Mutex<u64>,
}

impl SchemaChangingTool {
    fn definition(&self) -> lash_core::ToolDefinition {
        let field = if *self.revision.lock_recover() == 1 {
            "first_value"
        } else {
            "second_value"
        };
        lash_core::ToolDefinition::raw(
            "tool:schema_changing",
            "schema_changing",
            "A stable member whose contract can refresh",
            serde_json::json!({
                "type": "object",
                "properties": { field: { "type": "string" } },
                "required": [field],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SchemaChangingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "schema_changing").then(|| Arc::new(self.definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::Value::Null).into()
    }
}

#[derive(Clone, Debug, Default)]
struct SpanCapture {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedSpan {
    name: String,
    parent: Option<String>,
}

impl SpanCapture {
    fn snapshot(&self) -> Vec<CapturedSpan> {
        self.spans.lock_recover().clone()
    }
}

impl<S> Layer<S> for SpanCapture
where
    S: ::tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        _attrs: &::tracing::span::Attributes<'_>,
        id: &::tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("new span present in registry");
        let captured = CapturedSpan {
            name: span.metadata().name().to_string(),
            parent: span
                .parent()
                .map(|parent| parent.metadata().name().to_string()),
        };
        self.spans.lock_recover().push(captured);
    }
}

/// An `echo_tool` whose Run-owned Deferred source is resolved out of band.
struct PendingEchoTool {
    resolver: ActorContext,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for PendingEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![pending_echo_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(pending_echo_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let key = call
            .context
            .completion_key()
            .expect("the owning Run supplies the call's completion key");
        let resolver = self.resolver.clone();
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        tokio::task::yield_now().await;
        let _ = lash_core::waits::resolve_host(
            resolver.backend(),
            key.as_str(),
            lash_core::Resolution::Ok(serde_json::json!({
                "payload": format!("raw:{value}")
            })),
        )
        .await;
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::default())
    }
}

fn pending_echo_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo_tool",
        "echo_tool",
        "Return a tool payload",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

/// FIG-5263: observation publication after durable admission uses the
/// adopted snapshot while resident services await reload, without a warning.
#[tokio::test]
async fn durable_observation_skips_invalidated_services_without_warning() {
    let (_, capture) = trace_capture::capturing(|| async {
        let backend = sqlite_memory_store_backend().await;
        let mut runtime = runtime_with_plugins_and_tools(
            &backend,
            Vec::new(),
            Arc::new(EmptyTools),
            mock_provider(Vec::new()),
        )
        .await;
        runtime.invalidate_resident_session_state();
        let handle = RuntimeHandle::new(runtime);
        assert!(handle.observe().plugin_services.is_none());
        let writer = handle.writer();
        let mut runtime = writer.lock().await;
        runtime
            .reload_invalidated_resident_session_state()
            .await
            .expect("reload");
        handle.adopt_observation_from(&runtime);
        assert!(
            matches!(
                runtime.resident_session.validity(),
                ResidentSessionState::Valid
            ),
            "reload adopts the durable head"
        );
    })
    .await;
    let warnings: Vec<_> = capture
        .events
        .lock_recover()
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event.target == "lash_core::runtime::observation"
                && event
                    .field("message")
                    .contains("failed to capture plugin query services")
        })
        .cloned()
        .collect();
    assert!(
        warnings.is_empty(),
        "normal durable invalidation warned: {warnings:?}"
    );
}
