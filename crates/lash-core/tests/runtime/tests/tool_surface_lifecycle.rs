use super::*;
use lash_core::ProcessEventLogTestSupport as _;
use lash_core::SessionCommitStore as _;
use lash_core::ToolProvider as _;
use lash_core::facade_support::{RuntimeSessionStateFacadeOps, ToolStateFacadeOps};
use lash_core::plugin::PluginSessionRequest;
use lash_core::plugin::{SessionAuthorityContext, StaticPluginFactory};
use lash_sansio::sync::MutexExt;

const SEED: u64 = 0x5_c402;

#[derive(Clone, Debug)]
struct DynamicToolSpec {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    finish_on_execute: bool,
}

impl DynamicToolSpec {
    const fn new(id: &'static str, name: &'static str, description: &'static str) -> Self {
        Self {
            id,
            name,
            description,
            finish_on_execute: false,
        }
    }

    const fn finishing(mut self) -> Self {
        self.finish_on_execute = true;
        self
    }

    fn definition(&self) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            self.id,
            self.name,
            self.description,
            lash_core::ToolDefinition::default_input_schema(),
            json!({ "type": "object", "additionalProperties": true }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
    }
}

#[derive(Default)]
struct DynamicToolSurface {
    tools: Mutex<Vec<DynamicToolSpec>>,
}

impl DynamicToolSurface {
    fn new(tools: Vec<DynamicToolSpec>) -> Self {
        Self {
            tools: Mutex::new(tools),
        }
    }

    fn replace(&self, tools: Vec<DynamicToolSpec>) {
        *self.tools.lock_recover() = tools;
    }

    fn tool(&self, name: &str) -> Option<DynamicToolSpec> {
        self.tools
            .lock_recover()
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for DynamicToolSurface {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.tools
            .lock_recover()
            .iter()
            .map(|tool| tool.definition().manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.tool(name)
            .map(|tool| Arc::new(tool.definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let Some(tool) = self.tool(call.name()) else {
                return lash_core::ToolOutcome::err_fmt(format_args!(
                    "dynamic tool `{}` is not live",
                    call.name()
                ));
            };
            let result = lash_core::ToolOutcome::ok(json!({
                "id": tool.id,
                "name": tool.name,
                "description": tool.description,
            }));
            if tool.finish_on_execute {
                result.with_control(lash_core::ToolControl::Finish {
                    value: lash_core::ToolValue::untrusted_json(json!(tool.id)),
                })
            } else {
                result
            }
        })
        .await
        .into()
    }
}

fn dynamic_plugin_host(
    provider: Arc<dyn lash_core::ToolProvider>,
) -> Arc<lash_core::facade_support::PluginHost> {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("dynamic_tool_surface"),
        lash_core::facade_support::PluginSpec::new().with_tool_provider(provider),
    )));
    Arc::new(lash_core::testing::test_plugin_host(factories))
}

struct AllowNamedProcess {
    allowed: Arc<std::sync::OnceLock<lash_core::ProcessId>>,
}

impl lash_core::facade_support::ProcessToolVisibilityFilter for AllowNamedProcess {
    fn narrow(
        &self,
        _session: &lash_core::SessionId,
        candidates: &[lash_core::ProcessId],
    ) -> Vec<lash_core::ProcessId> {
        let mut narrowed = candidates
            .iter()
            .filter(|process_id| Some(*process_id) == self.allowed.get())
            .cloned()
            .collect::<Vec<_>>();
        // A foreign id proves the runtime intersects the answer with the
        // already edge-visible candidate set instead of trusting widening.
        narrowed.push(lash_core::ProcessId::fixture("foreign-process"));
        narrowed
    }
}

fn hidden_authority(tool_name: &str) -> SessionAuthorityContext {
    SessionAuthorityContext {
        tool_access: lash_core::SessionToolAccess::ambient()
            .with_hidden_tools([tool_name])
            .expect("valid hidden name"),
        ..SessionAuthorityContext::default()
    }
}

fn build_hidden_session(
    plugin_host: &lash_core::facade_support::PluginHost,
    session_id: &SessionId,
    hidden_tool_name: &str,
    snapshot: Option<&lash_core::PluginState>,
) -> Arc<lash_core::facade_support::PluginSession> {
    let authority = hidden_authority(hidden_tool_name);
    match snapshot {
        Some(snapshot) => plugin_host.build_session(PluginSessionRequest::rematerialization(
            session_id, snapshot, authority,
        )),
        None => plugin_host.build_session(PluginSessionRequest::creation(session_id, authority)),
    }
    .expect("hidden child plugin session")
}

fn registry(runtime: &LashRuntime) -> Arc<lash_core::ToolRegistry> {
    runtime
        .session
        .as_ref()
        .expect("runtime session")
        .plugins()
        .tool_registry()
}

fn text_response(text: &str) -> TestProvider {
    mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }])
}

#[tokio::test(flavor = "multi_thread")]
async fn broader_authority_fork_regains_parent_hidden_tool() {
    let hidden = DynamicToolSpec::new(
        "tool:broader_fork",
        "broader_fork",
        "hidden only from the parent",
    );
    let provider: Arc<dyn lash_core::ToolProvider> =
        Arc::new(DynamicToolSurface::new(vec![hidden.clone()]));
    let plugin_host = dynamic_plugin_host(provider);
    let parent = build_hidden_session(
        plugin_host.as_ref(),
        &SessionId::from("narrow-parent"),
        hidden.name,
        None,
    );
    assert!(
        parent
            .tool_registry()
            .export_state()
            .get(&lash_core::ToolId::from(hidden.id))
            .expect("parent hidden entry")
            .is_member(),
        "the parent's authority must not become inherited curation"
    );

    let child = parent
        .fork_for_session(
            "broader-child",
            lash_core::plugin::SessionAuthorityContext::default(),
        )
        .expect("fork with broader child authority");
    let session = lash_core::testing::runtime_internals::Session::new(
        lash_core::testing::runtime_services_without_ports(child),
        &SessionId::from("broader-child"),
    )
    .await
    .expect("broader child session");
    let surface = session
        .pin_tool_surface(&lash_core::SessionToolAccess::ambient())
        .expect("broader child request surface");

    assert!(
        surface.tool_catalog().has_callable_tool(hidden.name),
        "the child re-derives authority instead of inheriting the parent's hide"
    );
    assert!(surface.tools().resolve_manifest(hidden.name).is_some());
}

#[path = "../../runtime_support/payload_gated_engine.rs"]
mod payload_gated_engine;
use payload_gated_engine::{PAYLOAD_GATED_ENGINE_KIND, PayloadGatedEngine};

include!("../../runtime_support/process_start_admission.rs");
