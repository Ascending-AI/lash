//! Host tools the facade laws share.

use super::*;

/// `surface_test`: its assistant-response transform reports a
/// `PluginRuntime` status event on every turn.
pub(crate) struct SurfacePluginFactory;

impl PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<crate::plugins::PluginRecipe, crate::plugins::PluginError> {
        let mut reg = crate::plugins::PluginRegistrar::<SurfacePlugin>::new(
            crate::plugins::PluginFactory::id(self.as_ref()),
        );
        SurfacePlugin::declare(&mut reg)?;
        let id = self.id();
        Ok(crate::plugins::PluginRecipe::new(
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
        reg: &mut crate::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), crate::plugins::PluginError> {
        reg.output().response(
            crate::hook_key!("output-response-1"),
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
pub(crate) struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, crate::plugins::PluginError> {
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
pub(crate) fn role_and_text(message: &lash_core::Message) -> (String, String) {
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
