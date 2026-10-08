//! Host tools the facade laws share.

use super::*;
use lash_core::ToolDefinitionBindingExt as _;

/// `app_lookup`, a deferring host tool that answers `{ "ok": true }`.
pub(crate) struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

fn app_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas"),
        "app_lookup",
    )
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

/// A host sink: every activity it was handed, in order.
#[derive(Default)]
pub(crate) struct RecordingEvents {
    events: tokio::sync::Mutex<Vec<crate::TurnActivity>>,
}

impl RecordingEvents {
    pub(crate) async fn snapshot(&self) -> Vec<crate::TurnActivity> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl crate::TurnActivitySink for RecordingEvents {
    async fn emit(&self, activity: crate::TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

/// `surface_test`: its assistant-response transform reports a
/// `PluginRuntime` status event on every turn.
pub(crate) struct SurfacePluginFactory;

impl PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(SurfacePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for SurfacePluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("surface_test")
    }
}

struct SurfacePlugin;

impl lash_core::facade_support::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.output().response(
            crate::hook_key!("output-response-1"),
            None,
            Arc::new(|ctx| {
                Box::pin(async move {
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response: ctx.response,
                        events: vec![lash_core::PluginRuntimeEvent::Status {
                            key: "surface".to_string(),
                            label: "working".to_string(),
                            detail: Some("details".to_string()),
                        }],
                    })
                })
            }),
        )?;
        Ok(())
    }
}

/// `app_lookup` answering a 36-character string.
pub(crate) struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![long_text_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(long_text_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789")).into()
    }
}

fn long_text_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up verbose app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas"),
        "app_lookup",
    )
}

/// A model that answers each call with the next of `texts`.
#[cfg(feature = "rlm")]
pub(crate) fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(StdMutex::new(std::collections::VecDeque::from(
        texts
            .into_iter()
            .map(|text| text_response(&text.into()))
            .collect::<Vec<_>>(),
    )));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let next = responses
                .lock_recover()
                .pop_front()
                .expect("queued response");
            async move { Ok(next) }
        })
        .build()
        .into_handle()
}

/// `source` as one TypeScript cell.
#[cfg(feature = "rlm")]
pub(crate) fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}
