#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_protocol_types::sync::MutexExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::LashCore;
use lash::TurnInput;
use lash::direct::LlmOutputPart;
use lash::plugins::{PluginFactory, SessionPlugin};
use lash::provider::LlmResponse;
use lash::tools::{
    ToolAttemptOutcome, ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome,
    ToolProvider,
};
use serde_json::json;

fn assistant_prose(result: &lash::turn::TurnOutput) -> String {
    result
        .outcome
        .assistant_message()
        .unwrap_or_default()
        .to_string()
}

const TEST_PLUGIN_ID: &str = "test_typed";

/// The plugin's recorded namespace: the label a session is created with.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
struct TestPluginConfig {
    label: String,
}

/// The creator states the label; a session that states none records none.
struct TestConfigOwner;

impl lash::plugins::ConfigOwner for TestConfigOwner {
    type Create = TestPluginConfig;
    type Recorded = TestPluginConfig;
    type Refusal = String;
    type RunOptions = lash::plugins::NoRunOptions;

    fn create(&self, input: Option<TestPluginConfig>) -> Result<Option<TestPluginConfig>, String> {
        Ok(input)
    }

    fn validate(
        &self,
        _value: &TestPluginConfig,
        _base: Option<&TestPluginConfig>,
        _facts: &lash::plugins::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &Self::Recorded,
        _options: Self::RunOptions,
    ) -> std::result::Result<Self::Recorded, Self::Refusal> {
        Ok(recorded.clone())
    }
}

/// Installed on the core: every session runs it, configured by the label its
/// creation recorded.
#[derive(Default)]
struct TestPluginFactory {
    hook_seen: Arc<Mutex<Vec<String>>>,
    tool_seen: Arc<Mutex<Vec<String>>>,
}

impl PluginFactory for TestPluginFactory {
    fn id(&self) -> &'static str {
        TEST_PLUGIN_ID
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<
            lash_core::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > = {
            let mut reg = lash_core::plugin::PluginRegistrar::<TestSessionPlugin>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            TestSessionPlugin::declare(&mut reg)?;
            let id = lash_core::plugin::PluginFactory::id(self.as_ref());
            Ok(lash_core::plugin::PluginRecipe::new(
                id,
                reg,
                move |ctx, _state| {
                    let label = ctx
                        .plugin_config
                        .config
                        .decode::<TestPluginConfig>(TEST_PLUGIN_ID)
                        .map_err(|error| {
                            lash_execution_contracts::plugin_error::PluginError::Session(
                                error.to_string(),
                            )
                        })?
                        .map(|config| config.label);
                    Ok(Arc::new(TestSessionPlugin {
                        label,
                        hook_seen: Arc::clone(&self.hook_seen),
                        tool_seen: Arc::clone(&self.tool_seen),
                    }))
                },
            ))
        };
        let mut recipe = recipe?;
        let mut configuration = lash_core::plugin::ConfigRegistrar::new(
            lash_core::plugin::PluginFactory::id(recipe_factory.as_ref()),
        );
        recipe_factory.declare_configuration(&mut configuration)?;
        recipe = recipe.with_configuration(configuration);
        Ok(recipe)
    }
}
impl TestPluginFactory {
    pub fn declare_configuration(
        &self,
        registrar: &mut lash::plugins::ConfigRegistrar,
    ) -> Result<(), lash::plugins::ConfigRegistrationError> {
        registrar.owner(TestConfigOwner)
    }
}

impl TestPluginFactory {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(TEST_PLUGIN_ID)
    }
}
impl lash_core::plugin::PluginMetadata for TestPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct TestSessionPlugin {
    /// The label the session recorded, if it stated one.
    label: Option<String>,
    hook_seen: Arc<Mutex<Vec<String>>>,
    tool_seen: Arc<Mutex<Vec<String>>>,
}

impl SessionPlugin for TestSessionPlugin {
    fn id(&self) -> &'static str {
        TEST_PLUGIN_ID
    }
}
impl TestSessionPlugin {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.turn()
            .before(lash::hook_key!("turn-before-1"), move |this, _state| {
                let hook_seen = Arc::clone(&this.hook_seen);
                let hook_label = this.label.clone();
                Ok(Arc::new(move |_ctx| {
                    let hook_seen = Arc::clone(&hook_seen);
                    let label = hook_label.clone();
                    Box::pin(async move {
                        if let Some(label) = label {
                            hook_seen.lock_recover().push(label);
                        }
                        Ok(Default::default())
                    })
                }))
            })?;

        reg.tools()
            .provider(lash::tool_provider_key!("tools"), move |this, _state| {
                Ok(Arc::new(TestTools {
                    label: this.label.clone(),
                    seen: Arc::clone(&this.tool_seen),
                }))
            })?;
        Ok(())
    }
}

struct TestTools {
    label: Option<String>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ToolProvider for TestTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok({
            if self.label.is_some() {
                vec![typed_probe_definition().manifest()]
            } else {
                Vec::new()
            }
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        (name == "tools.typed_probe").then(|| Arc::new(typed_probe_definition().contract()))
    }

    async fn prepare_tool_call(
        &self,
        call: lash::tools::ToolPrepareCall<'_>,
    ) -> Result<lash::tools::PreparedToolCall, ToolOutcome> {
        if call.pending.tool_name != "tools.typed_probe" {
            return Ok(lash::tools::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ));
        }
        let prepared_payload = json!({ "label": self.label });
        Ok(lash::tools::PreparedToolCall {
            call_id: call.pending.call_id,
            provider_call_id: call.pending.provider_call_id,
            tool_id: call.tool_id,
            tool_name: call.pending.tool_name,
            args: call.pending.args,
            replay: call.pending.replay,
            prepared_payload,
        })
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "tools.typed_probe");
            let input = match call.context.decode_prepared_payload::<serde_json::Value>() {
                Ok(input) => input,
                Err(err) => {
                    return ToolOutcome::err_fmt(format!("missing prepared typed input: {err}"));
                }
            };
            let label = input
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            self.seen.lock_recover().push(label.clone());
            ToolOutcome::ok(json!({ "label": label }))
        })
        .await
        .into()
    }
}

fn typed_probe_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:typed_probe",
        "tools.typed_probe",
        "Probe typed turn input.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

fn response_text(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn response_tool_call() -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "tool-1".to_string(),
            tool_name: "tools__typed_probe".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

async fn core_with_responses(
    responses: Vec<LlmResponse>,
    plugin: Arc<TestPluginFactory>,
) -> LashCore {
    let responses = Arc::new(Mutex::new(responses.into_iter()));
    let provider = lash_core::testing::TestProvider::builder()
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move {
                Ok(responses
                    .lock_recover()
                    .next()
                    .unwrap_or_else(|| response_text("fallback")))
            }
        })
        .build()
        .into_handle();
    LashCore::standard_builder(crate::support::sqlite_memory_store_backend().await)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "mock-model",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("mock-model")
                            .cache_retention(lash::provider::CacheRetention::Short)
                            .context_window_tokens(16_000)
                            .build()
                            .expect("valid model spec"),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .plugin(plugin)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("embed-plugins-test-worker"),
            lash::persistence::LeaseIncarnationId::new("embed-plugins-test-boot"),
        ))
        .expect("core")
}

/// Creates `session_id` recording `label` as the plugin's namespace, or
/// stating none.
async fn created_with_label(core: &LashCore, session_id: &str, label: Option<&str>) {
    let plugin_options = match label {
        Some(label) => lash::plugins::PluginOptions::typed(
            TEST_PLUGIN_ID,
            TestPluginConfig {
                label: label.to_string(),
            },
        )
        .expect("encode the plugin's creation options"),
        None => lash::plugins::PluginOptions::default(),
    };
    core.session(lash_vocabulary::identity::SessionId::fixture(
        session_id.to_string(),
    ))
    .create(lash::SessionCreation::root(
        lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        )
        .plugin_options(plugin_options),
    ))
    .await
    .expect("create the session");
}

#[tokio::test]
async fn turn_hook_and_tool_provider_read_recorded_session_config() {
    let plugin = Arc::new(TestPluginFactory::default());
    let core = core_with_responses(
        vec![response_tool_call(), response_text("done")],
        Arc::clone(&plugin),
    )
    .await;
    created_with_label(&core, "typed-context", Some("page-a")).await;
    let session = core
        .session(
            lash_vocabulary::identity::SessionId::parse("typed-context")
                .expect("nonblank host identity"),
        )
        .open()
        .await
        .expect("session");

    let result = session
        .send(TurnInput::text("probe"))
        .output()
        .await
        .expect("turn");

    assert_eq!(assistant_prose(&result), "done");
    // The before-turn hook runs once for the run, under the label its
    // session recorded.
    assert_eq!(plugin.hook_seen.lock_recover().as_slice(), ["page-a"]);
    assert_eq!(plugin.tool_seen.lock_recover().as_slice(), ["page-a"]);
}

#[tokio::test]
async fn sessions_that_record_no_plugin_config_do_not_get_inactive_fallback_tools() {
    let plugin = Arc::new(TestPluginFactory::default());
    let core = core_with_responses(vec![response_text("done")], plugin).await;
    created_with_label(&core, "without-typed-config", None).await;
    let session = core
        .session(
            lash_vocabulary::identity::SessionId::parse("without-typed-config")
                .expect("nonblank host identity"),
        )
        .open()
        .await
        .expect("session");

    let commands = session.admin().commands();
    let receipt = commands
        .refresh_tool_catalog("inspect the native tool surface", "native-tool-surface")
        .await
        .expect("submit native capability command");
    commands
        .settle(receipt)
        .await
        .expect("native capability command settles");

    let definitions = session.admin().offered_tools().await.expect("definitions");

    assert!(
        definitions
            .iter()
            .all(|definition| definition.tool() != "tools.typed_probe")
    );
}
