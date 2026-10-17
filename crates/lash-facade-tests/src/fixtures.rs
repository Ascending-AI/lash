//! Host tools the facade laws share.

use super::*;

/// `surface_test`: its assistant-response transform reports a
/// `PluginRuntime` status event on every turn.
pub struct SurfacePluginFactory;

impl PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<SurfacePlugin>::new(
            lash::plugins::PluginFactory::id(self.as_ref()),
        );
        SurfacePlugin::declare(&mut reg)?;
        let id = self.id();
        Ok(lash::plugins::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(SurfacePlugin)),
        ))
    }
}

impl SurfacePluginFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("surface_test")
    }
}
impl lash_core::plugin::PluginMetadata for SurfacePluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct SurfacePlugin;

impl lash_core::facade_support::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }
}
impl SurfacePlugin {
    fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        reg.output().response(
            lash::hook_key!("output-response-1"),
            None,
            move |_this, _state| {
                Ok(Arc::new(|ctx| {
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
                }))
            },
        )?;
        Ok(())
    }
}

/// `app_lookup` answering a 36-character string.
pub struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![long_text_tool_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
        "app_lookup",
    )
}

/// A committed message as the store-level laws compare it: its role's name
/// and its parts' content, one per line.
pub fn role_and_text(message: &lash_core::Message) -> (String, String) {
    let role = match message.role {
        lash_core::MessageRole::User => "user",
        lash_core::MessageRole::Assistant => "assistant",
        lash_core::MessageRole::System => "system",
        lash_core::MessageRole::Event => "event",
    };
    let text = message
        .parts
        .iter()
        .map(|part| part.content())
        .collect::<Vec<_>>()
        .join("\n");
    (role.to_owned(), text)
}

pub const DIRECT_PROMPT: &str = "raw prompt";

fn direct_probe_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:direct_probe",
        "direct_probe",
        "Ask the model directly and answer what it said.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A tool whose call asks the model [`DIRECT_PROMPT`] directly.
pub struct DirectProbe;

#[async_trait]
impl ToolProvider for DirectProbe {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![direct_probe_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "direct_probe").then(|| Arc::new(direct_probe_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(DIRECT_PROMPT),
                "direct-llm-test",
            )
            .await
        {
            Ok(completion) => lash_core::ToolOutcome::ok(serde_json::json!({
                "text": completion.text,
                "input_tokens": completion.usage.input_tokens,
                "output_tokens": completion.usage.output_tokens,
                "attempts": completion.llm_call.attempts.len(),
            }))
            .into(),
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}
