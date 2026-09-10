use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::TurnInput;
use lash::direct::LlmOutputPart;
use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash::provider::LlmResponse;
use lash::tools::{
    ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider,
};
use lash::{LashCore, PluginBinding};
use serde_json::json;

fn assistant_prose(result: &lash::turn::TurnOutput) -> String {
    result
        .result
        .assistant_message()
        .unwrap_or_default()
        .to_string()
}

#[derive(Clone, Debug)]
struct TestPlugin;

#[derive(Clone)]
struct TestPluginConfig {
    label: String,
    prompt_seen: Arc<Mutex<Vec<String>>>,
    tool_seen: Arc<Mutex<Vec<String>>>,
}

impl PluginBinding for TestPlugin {
    const ID: &'static str = "test_typed";
    type SessionConfig = TestPluginConfig;

    fn factory(config: &Self::SessionConfig) -> Arc<dyn PluginFactory> {
        Arc::new(TestPluginFactory {
            config: config.clone(),
        })
    }
}

struct TestPluginFactory {
    config: TestPluginConfig,
}

impl PluginFactory for TestPluginFactory {
    fn id(&self) -> &'static str {
        TestPlugin::ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(TestSessionPlugin {
            config: self.config.clone(),
        }))
    }
}

struct TestSessionPlugin {
    config: TestPluginConfig,
}

impl SessionPlugin for TestSessionPlugin {
    fn id(&self) -> &'static str {
        TestPlugin::ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let prompt_seen = Arc::clone(&self.config.prompt_seen);
        let label = self.config.label.clone();
        reg.prompt().contribute(Arc::new(move |_ctx| {
            let prompt_seen = Arc::clone(&prompt_seen);
            let label = label.clone();
            Box::pin(async move {
                prompt_seen.lock_recover().push(label);
                Ok(Vec::new())
            })
        }));
        reg.tools().provider(Arc::new(TestTools {
            label: self.config.label.clone(),
            seen: Arc::clone(&self.config.tool_seen),
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
        Ok(lash::tools::PreparedToolCall::from_parts(
            call.pending.call_id,
            call.tool_id,
            call.pending.tool_name,
            call.pending.args,
            call.pending.replay,
            prepared_payload,
        ))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        assert_eq!(call.name, "typed_probe");
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

fn core_with_responses(responses: Vec<LlmResponse>) -> LashCore {
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
    LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("mock-model")
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
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "embed-plugins-test-worker",
            "embed-plugins-test-boot",
        ))
        .expect("core")
}

#[tokio::test]
async fn prompt_hook_and_tool_provider_read_typed_session_config() {
    let prompt_seen = Arc::new(Mutex::new(Vec::new()));
    let tool_seen = Arc::new(Mutex::new(Vec::new()));
    let config = TestPluginConfig {
        label: "page-a".to_string(),
        prompt_seen: Arc::clone(&prompt_seen),
        tool_seen: Arc::clone(&tool_seen),
    };
    let core = core_with_responses(vec![response_tool_call(), response_text("done")]);
    let session = core
        .session("typed-context")
        .plugin::<TestPlugin>(config)
        .open()
        .await
        .expect("session");

    let result = session
        .turn(TurnInput::text("probe"))
        .run()
        .await
        .expect("turn");

    assert_eq!(assistant_prose(&result), "done");
    assert_eq!(prompt_seen.lock_recover().as_slice(), ["page-a", "page-a"]);
    assert_eq!(tool_seen.lock_recover().as_slice(), ["page-a"]);
}

#[tokio::test]
async fn sessions_without_typed_plugin_install_do_not_get_inactive_fallback_tools() {
    let core = core_with_responses(vec![response_text("done")]);
    let session = core
        .session("without-typed-plugin")
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
