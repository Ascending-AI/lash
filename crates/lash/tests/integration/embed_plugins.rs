#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::LashCore;
use lash::TurnInput;
use lash::direct::LlmOutputPart;
use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash::provider::LlmResponse;
use lash::tools::{
    ToolAttemptOutcome, ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome,
    ToolProvider,
};
use serde_json::json;

const SEED: u64 = 0x5c_f10b;

fn assistant_prose(result: &lash::turn::TurnOutput) -> String {
    result
        .result
        .assistant_message()
        .unwrap_or_default()
        .to_string()
}

const TEST_PLUGIN_ID: &str = "test_typed";

/// The plugin's recorded namespace: the label a session is created with.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, lash::plugins::JsonSchema)]
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

    fn implementation(&self) -> &str {
        "test_typed:1"
    }

    fn create(
        &self,
        input: Option<TestPluginConfig>,
        _facts: lash::plugins::CreationFacts<'_, TestPluginConfig>,
    ) -> Result<Option<TestPluginConfig>, String> {
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

    fn declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(self.id())
    }

    fn register_config(
        &self,
        registrar: &mut lash::plugins::ConfigRegistrar,
    ) -> Result<(), lash::plugins::ConfigRegistrationError> {
        registrar.owner(TestConfigOwner)
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        let label = ctx
            .plugin_config
            .config
            .decode::<TestPluginConfig>(TEST_PLUGIN_ID)
            .map_err(|error| PluginError::Session(error.to_string()))?
            .map(|config| config.label);
        Ok(Arc::new(TestSessionPlugin {
            label,
            hook_seen: Arc::clone(&self.hook_seen),
            tool_seen: Arc::clone(&self.tool_seen),
        }))
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

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        // A session that recorded no label runs the plugin with nothing to
        // contribute: no turn hook and no tools.
        let Some(label) = self.label.clone() else {
            return Ok(());
        };
        let hook_seen = Arc::clone(&self.hook_seen);
        let hook_label = label.clone();
        reg.turn().before(Arc::new(move |_ctx| {
            let hook_seen = Arc::clone(&hook_seen);
            let label = hook_label.clone();
            Box::pin(async move {
                hook_seen.lock_recover().push(label);
                Ok(Vec::new())
            })
        }));
        reg.tools().provider(Arc::new(TestTools {
            label,
            seen: Arc::clone(&self.tool_seen),
        }))
    }
}

struct TestTools {
    label: String,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ToolProvider for TestTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![typed_probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "typed_probe").then(|| Arc::new(typed_probe_definition().contract()))
    }

    async fn prepare_tool_call(
        &self,
        call: lash::tools::ToolPrepareCall<'_>,
    ) -> Result<lash::tools::PreparedToolCall, ToolOutcome> {
        if call.pending.tool_name != "typed_probe" {
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
            assert_eq!(call.name(), "typed_probe");
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
        "typed_probe",
        "Probe typed turn input.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        json!({ "type": "object" }),
    )
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
            tool_name: "typed_probe".to_string(),
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
) -> (LashCore, lash_restate_test::RestateTestBackend) {
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
    let double = crate::support::restate_double(SEED).await;
    let core = LashCore::standard_builder(double.lash_backend())
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "mock-model",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("mock-model")
                            .context_window_tokens(16_000)
                            .build()
                            .expect("valid model spec"),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .plugin(plugin)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "embed-plugins-test-worker",
            "embed-plugins-test-boot",
        ))
        .expect("core");
    (core, double)
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
    core.session(session_id)
        .create(lash::SessionCreation::root(
            lash::SessionSpec::new(
                "mock-model",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .plugin_options(plugin_options),
        ))
        .await
        .expect("create the session");
}

#[tokio::test]
async fn turn_hook_and_tool_provider_read_recorded_session_config() {
    let plugin = Arc::new(TestPluginFactory::default());
    let (core, _double) = core_with_responses(
        vec![response_tool_call(), response_text("done")],
        Arc::clone(&plugin),
    )
    .await;
    created_with_label(&core, "typed-context", Some("page-a")).await;
    let session = core.session("typed-context").open().await.expect("session");

    let result = session
        .send(TurnInput::text("probe"))
        .output()
        .await
        .expect("turn");

    assert_eq!(assistant_prose(&result), "done");
    // The before-turn hook runs once for the root, under the label its
    // session recorded.
    assert_eq!(plugin.hook_seen.lock_recover().as_slice(), ["page-a"]);
    assert_eq!(plugin.tool_seen.lock_recover().as_slice(), ["page-a"]);
}

#[tokio::test]
async fn sessions_that_record_no_plugin_config_do_not_get_inactive_fallback_tools() {
    let plugin = Arc::new(TestPluginFactory::default());
    let (core, _double) = core_with_responses(vec![response_text("done")], plugin).await;
    created_with_label(&core, "without-typed-config", None).await;
    let session = core
        .session("without-typed-config")
        .open()
        .await
        .expect("session");

    let definitions = session
        .admin()
        .tools()
        .active_manifests()
        .await
        .expect("definitions");

    assert!(
        definitions
            .iter()
            .all(|definition| definition.name != "typed_probe")
    );
}
