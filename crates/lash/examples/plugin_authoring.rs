//! Compiled specimens for docs/plugins.md. Plugins are fully trusted Rust.
use lash::plugins::*;
use lash::prompt::{PromptPlacement, PromptSectionKey};
use lash::tools::*;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const GREETINGS: &str = "greetings";
struct Prefix;
impl ConfigOwner for Prefix {
    type Create = String;
    type Recorded = String;
    type Refusal = String;
    type RunOptions = NoRunOptions;
    fn create(&self, input: Option<String>) -> Result<Option<String>, String> {
        Ok(Some(input.unwrap_or_else(|| "friend".into())))
    }
    fn validate(
        &self,
        prefix: &String,
        _: Option<&String>,
        _: &CandidateFacts<'_>,
    ) -> Result<(), String> {
        if prefix.len() <= 128 {
            Ok(())
        } else {
            Err("prefix is too long".into())
        }
    }
    fn apply_run_options(&self, _: &String, options: NoRunOptions) -> Result<String, String> {
        match options {}
    }
}
struct Greetings(String);
#[lash::async_trait]
impl StaticToolExecute for Greetings {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        ToolOutcome::ok(json!(format!("{} {}", call.name(), self.0))).into()
    }
}
/// Two fixed tools share recorded, creation-only session configuration.
pub fn greetings() -> Result<Arc<dyn PluginFactory>, PluginError> {
    let definitions = ["hello", "goodbye"]
        .into_iter()
        .map(|name| {
            ToolDefinition::raw(
                format!("greetings.{name}"),
                format!("greetings.{name}"),
                name,
                json!({"type":"object","properties":{},"additionalProperties":false}),
                json!({"type":"string"}),
            )
            .map(|draft| draft.with_execution(Duration::from_secs(1)))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| PluginError::Registration(error.to_string()))?;
    let mut entries = PluginRegistrar::<String>::new(GREETINGS);
    entries
        .tools()
        .provider(lash::tool_provider_key!("tools"), move |prefix, _| {
            Ok(Arc::new(StaticToolProvider::new(
                definitions.clone(),
                Greetings(prefix.clone()),
            )))
        })?;
    let recipe = PluginRecipe::bound(GREETINGS, entries, |context, _| {
        let prefix = context
            .plugin_config
            .config
            .decode::<String>(GREETINGS)
            .map_err(|error| PluginError::Registration(error.to_string()))?
            .unwrap_or_default();
        Ok(Arc::new(prefix))
    });
    let mut configuration = ConfigRegistrar::new(GREETINGS);
    configuration.owner(Prefix)?;
    let recipe = recipe.with_configuration(configuration);
    Ok(Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial(GREETINGS),
        recipe,
    )))
}

/// A keyed check reads the provider-prepared call and returns a typed verdict.
pub fn protected_path() -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("protected-path"),
        PluginSpec::new().with_tool_args_check(
            lash::hook_key!("policy"),
            Arc::new(|input| {
                Box::pin(async move {
                    let blocked = input.prepared.tool_name() == "tools.write"
                        && input
                            .prepared
                            .args()
                            .get("path")
                            .and_then(|value| value.as_str())
                            == Some(".env");
                    Ok(if blocked {
                        BeforeToolDecision::Deny(ToolFailure::invalid_request(
                            "protected_path",
                            "Blocked .env",
                        ))
                    } else {
                        BeforeToolDecision::Allow
                    })
                })
            }),
        ),
    ))
}

/// The reducer publishes a counter; the prompt reads the committed cut.
pub fn counter() -> Result<Arc<dyn PluginFactory>, PluginError> {
    let mut entries = PluginRegistrar::new("counter");
    entries.state_reducer("increment", |_, _| {
        Ok(Arc::new(|input| {
            Ok(Some(json!(
                input.current.and_then(|value| value.as_u64()).unwrap_or(0) + 1
            )))
        }))
    })?;
    entries
        .turn()
        .before(lash::hook_key!("increment"), |_, _| {
            Ok(Arc::new(|_| {
                Box::pin(async {
                    Ok(TurnContributions {
                        state: StateCommands::new().apply("count", "increment", json!(null)),
                        ..Default::default()
                    })
                })
            }))
        })?;
    entries.prompt().section(
        PromptSectionSpec::new(
            PromptSectionKey::new("counter")
                .map_err(|error| PluginError::Registration(error.to_string()))?,
            PromptPlacement::InitialInstructions,
        ),
        |_, _| {
            Ok(Arc::new(|input: &PromptInput<'_>| {
                Ok(SectionText::text(format!(
                    "Accepted turns: {}",
                    input.state().get("count").unwrap_or(&json!(0))
                )))
            }))
        },
    )?;
    let recipe = PluginRecipe::from_registrar("counter", entries);
    Ok(Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("counter"),
        recipe,
    )))
}

/// A host-generic example publishing a value above the standard 32 KiB bound.
pub fn large_state_factory() -> Result<Arc<dyn PluginFactory>, PluginError> {
    let mut entries = PluginRegistrar::new("large-state");
    entries.turn().before(lash::hook_key!("publish"), |_, _| {
        Ok(Arc::new(|_| {
            Box::pin(async {
                Ok(TurnContributions {
                    state: StateCommands::new().set("value", json!("x".repeat(64 * 1024))),
                    ..Default::default()
                })
            })
        }))
    })?;
    Ok(Arc::new(PluginRecipeFactory::new(
        PluginDeclaration::initial("large-state"),
        PluginRecipe::from_registrar("large-state", entries),
    )))
}
pub async fn large_state() -> Result<(), Box<lash::testing::PluginHarnessError>> {
    let factory = large_state_factory().map_err(lash::testing::PluginHarnessError::from)?;
    let mut policy = lash::testing::HostPolicy::standard();
    policy.data_limits.plugin_state.max_value_bytes = 128 * 1024;
    let mut harness = lash::testing::PluginHarness::new(factory, policy).await?;
    harness.create(PluginOptions::default()).await?;
    harness.script([lash::testing::text_response("published")]);
    harness
        .publish_then_reopen(lash::TurnInput::text("publish"))
        .await?;
    let state = harness.plugin_state().await?;
    assert_eq!(
        state.plugins["large-state"].values["value"],
        json!("x".repeat(64 * 1024))
    );
    Ok(())
}
#[cfg(not(test))]
fn main() {}
