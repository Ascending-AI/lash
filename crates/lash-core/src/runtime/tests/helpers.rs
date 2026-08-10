use super::*;
use lash_sansio::sync::MutexExt;

// The public in-memory store is the single in-memory `RuntimePersistence` impl;
// tests use it under the historical `RecordingStore` name (its `pub(crate)`
// fields + recording-count getters back the existing assertions).
pub(crate) use crate::runtime::in_memory_store::InMemorySessionStore as RecordingStore;

pub(crate) fn default_state() -> RuntimeSessionState {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.ensure_agent_frame_initialized();
    state
}

pub(crate) fn inline_scope(scope: crate::ExecutionScope) -> crate::ScopedEffectController<'static> {
    crate::ScopedEffectController::shared(
        Arc::new(crate::InlineRuntimeEffectController::default()),
        scope,
    )
    .expect("inline execution scope")
}

pub(crate) fn named_turn_scope(
    session_id: &str,
    turn_id: &str,
) -> crate::ScopedEffectController<'static> {
    inline_scope(crate::ExecutionScope::turn(session_id, turn_id))
}

#[test]
pub(crate) fn stream_accumulator_merges_adjacent_display_reasoning_chunks() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(" check".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(" the time.".to_string(), None, Vec::new(), None);

    assert_eq!(accumulator.parts.len(), 1);
    assert!(matches!(
        &accumulator.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
    ));
}

#[test]
pub(crate) fn stream_accumulator_enriches_reasoning_delta_with_later_roundtrip_payload() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
    accumulator.push_reasoning(
        "I'll check the time.".to_string(),
        Some("rs_1".to_string()),
        vec!["I'll check the time.".to_string()],
        Some("encrypted".to_string()),
    );

    assert_eq!(accumulator.parts.len(), 1);
    assert!(matches!(
        &accumulator.parts[0],
        LlmOutputPart::Reasoning {
            text,
            replay: Some(replay),
            ..
        } if text == "I'll check the time."
            && replay.item_id.as_deref() == Some("rs_1")
            && replay.encrypted_content.as_deref() == Some("encrypted")
    ));
}

#[test]
pub(crate) fn stream_accumulator_preserves_reasoning_when_final_response_has_tool_call() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll check the time.".to_string(), None, Vec::new(), None);
    accumulator.push_tool_call(
        "call_1".to_string(),
        "exec_command".to_string(),
        "{\"cmd\":\"date\"}".to_string(),
        Some(lash_sansio::llm::types::ProviderReplayMeta {
            item_id: Some("item_1".to_string()),
            opaque: Some("sig".to_string()),
        }),
    );

    let mut response = LlmResponse {
        full_text: String::new(),
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "call_1".to_string(),
            tool_name: "exec_command".to_string(),
            input_json: "{\"cmd\":\"date\"}".to_string(),
            replay: Some(lash_sansio::llm::types::ProviderReplayMeta {
                item_id: Some("item_1".to_string()),
                opaque: Some("sig".to_string()),
            }),
        }],
        response_metadata: Default::default(),
        ..Default::default()
    };

    accumulator.apply_to_response(&mut response);

    assert_eq!(response.parts.len(), 2);
    assert!(matches!(
        &response.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll check the time."
    ));
    assert!(matches!(
        &response.parts[1],
        LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "exec_command"
    ));
}

#[test]
pub(crate) fn stream_accumulator_does_not_duplicate_complete_final_response() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_reasoning("I'll answer.".to_string(), None, Vec::new(), None);
    accumulator.push_text("Done.");

    let mut response = LlmResponse {
        full_text: "Done.".to_string(),
        parts: vec![
            LlmOutputPart::Reasoning {
                text: "I'll answer.".to_string(),
                replay: None,
            },
            LlmOutputPart::Text {
                text: "Done.".to_string(),
                response_meta: None,
            },
        ],
        response_metadata: Default::default(),
        ..Default::default()
    };

    accumulator.apply_to_response(&mut response);

    assert_eq!(response.parts.len(), 2);
    assert!(matches!(
        &response.parts[0],
        LlmOutputPart::Reasoning { text, .. } if text == "I'll answer."
    ));
    assert!(matches!(
        &response.parts[1],
        LlmOutputPart::Text { text, .. } if text == "Done."
    ));
}

pub(crate) trait ReadModelState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel;
}

impl ReadModelState for SessionSnapshot {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
    }
}

impl ReadModelState for RuntimeSessionState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
    }
}

pub(crate) trait ReadModelStateMut: ReadModelState {
    fn append_message(&mut self, message: Message);
}

impl ReadModelStateMut for SessionSnapshot {
    fn append_message(&mut self, message: Message) {
        self.session_graph.append_message(message);
    }
}

impl ReadModelStateMut for RuntimeSessionState {
    fn append_message(&mut self, message: Message) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_message(message);
    }
}

pub(crate) fn active_conversation_messages(state: &impl ReadModelState) -> Vec<Message> {
    state.read_model().messages.as_ref().clone()
}

pub(crate) fn append_message(state: &mut impl ReadModelStateMut, message: Message) {
    state.append_message(message);
}

#[derive(Clone, Default)]
pub(crate) struct RecordingSink {
    pub(crate) events: Arc<Mutex<Vec<SessionStreamEvent>>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: SessionStreamEvent) {
        self.events.lock_recover().push(event);
    }
}

impl RecordingSink {
    pub(crate) fn snapshot(&self) -> Vec<SessionStreamEvent> {
        self.events.lock_recover().clone()
    }
}

#[derive(Clone, Default)]
pub(crate) struct RecordingTurnEvents {
    pub(crate) events: Arc<Mutex<Vec<TurnActivity>>>,
}

#[async_trait::async_trait]
impl TurnActivitySink for RecordingTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock_recover().push(activity);
    }
}

impl RecordingTurnEvents {
    pub(crate) fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock_recover().clone()
    }
}

#[derive(Debug)]
pub(crate) struct MockCall {
    pub(crate) stream_events: Vec<LlmStreamEvent>,
    pub(crate) response: Result<LlmResponse, LlmTransportError>,
}

pub(crate) fn mock_provider(calls: Vec<MockCall>) -> TestProvider {
    mock_provider_with_kind("mock", calls)
}

pub(crate) fn mock_openai_compatible_provider(calls: Vec<MockCall>) -> TestProvider {
    mock_provider_with_kind("openai-compatible", calls)
}

fn mock_provider_with_kind(kind: &'static str, calls: Vec<MockCall>) -> TestProvider {
    let calls = Arc::new(Mutex::new(calls));
    TestProvider::builder()
        .kind(kind)
        .requires_streaming(true)
        .complete(move |req| {
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.lock_recover().remove(0);
                if let Some(tx) = req.stream_events.as_ref() {
                    for event in &call.stream_events {
                        tx.send(event.clone());
                    }
                }
                call.response
            }
        })
        .build()
}

pub(crate) fn set_runtime_provider(runtime: &mut LashRuntime, provider: crate::ProviderHandle) {
    runtime.host.core.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider.clone()));
    runtime.policy.provider_id = provider.kind().to_string();
    runtime.state.policy.provider_id = provider.kind().to_string();
}

pub(crate) fn standard_test_policy() -> SessionPolicy {
    SessionPolicy {
        provider_id: "mock".to_string(),
        model: crate::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..SessionPolicy::new(crate::TurnBudget::Unbounded)
    }
}

pub(crate) fn test_host_config() -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(crate::CommitBudget::bounded(1024 * 1024, 512));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
}

pub(crate) fn test_host_config_with_trace_path(path: PathBuf) -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(crate::CommitBudget::bounded(1024 * 1024, 512));
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    EmbeddedRuntimeHost::new(config)
}

pub(crate) fn test_host_config_with_trace_path_and_stream_events(
    path: PathBuf,
) -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(crate::CommitBudget::bounded(1024 * 1024, 512));
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    config.tracing.trace_level = lash_trace::TraceLevel::Extended;
    EmbeddedRuntimeHost::new(config)
}

#[derive(Clone, Default)]
pub(crate) struct RecordingSessionStoreFactory {
    stores: Arc<StdMutex<Vec<Arc<RecordingStore>>>>,
    defer_metadata_to_admission: bool,
}

impl RecordingSessionStoreFactory {
    pub(crate) fn stores(&self) -> Vec<Arc<RecordingStore>> {
        self.stores.lock_recover().clone()
    }

    pub(crate) fn deferring_metadata_to_admission(mut self) -> Self {
        self.defer_metadata_to_admission = true;
        self
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RecordingSessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::store::RuntimePersistence>, crate::StoreError> {
        let store = Arc::new(RecordingStore::default());
        if !self.defer_metadata_to_admission {
            *store.session_meta.lock_recover() = Some(crate::SessionMeta {
                session_id: request.session_id.clone(),
                session_name: request.session_id.clone(),
                created_at: "2026-04-06T00:00:00Z".to_string(),
                model: request.policy.model.id.clone(),
                cwd: None,
                relation: request.relation.clone(),
            });
        }
        self.stores.lock_recover().push(Arc::clone(&store));
        Ok(store as Arc<dyn crate::store::RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, String> {
        Ok(self
            .stores
            .lock_recover()
            .iter()
            .find(|store| {
                store
                    .session_meta
                    .lock_recover()
                    .as_ref()
                    .is_some_and(|meta| meta.session_id == request.session_id)
            })
            .cloned()
            .map(|store| store as Arc<dyn crate::store::RuntimePersistence>))
    }

    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

pub(crate) fn plugin_session_with_tools(
    session_id: &str,
    tools: Arc<dyn crate::ToolProvider>,
) -> Arc<crate::PluginSession> {
    let tool_factory = StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(tool_factory));
    crate::PluginHost::new(factories)
        .build_session(session_id, None)
        .expect("plugins")
}

pub(crate) struct EmptyTools;

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        crate::ToolResult::err(serde_json::json!("Unknown tool"))
    }
}

pub(crate) async fn standard_runtime_with_transport(transport: TestProvider) -> LashRuntime {
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        test_host_config(),
        crate::RuntimeServices::new(plugin_session_with_tools("root", tools)),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
    )
    .await
    .expect("runtime");
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    set_runtime_provider(&mut runtime, transport.clone().into_handle());
    runtime
}
pub(crate) type RuntimeTestPluginBuilder = dyn Fn(&crate::PluginSessionContext) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError>
    + Send
    + Sync;
pub(crate) type RuntimeExternalRegistrar =
    dyn Fn(&mut crate::PluginRegistrar) -> Result<(), crate::PluginError> + Send + Sync;

pub(crate) struct RuntimeTestPluginFactory {
    pub(crate) build: Arc<RuntimeTestPluginBuilder>,
}

impl crate::PluginFactory for RuntimeTestPluginFactory {
    fn id(&self) -> &'static str {
        "runtime-test"
    }

    fn build(
        &self,
        ctx: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        (self.build)(ctx)
    }
}

pub(crate) struct RuntimeTestPlugin {
    pub(crate) before_turn: Option<crate::plugin::BeforeTurnHook>,
    pub(crate) checkpoint: Option<crate::plugin::CheckpointHook>,
    pub(crate) tool_result_projector: Option<crate::plugin::ToolResultProjector>,
    pub(crate) runtime_event: Option<crate::plugin::PluginLifecycleEventHook>,
    pub(crate) external_registrar: Option<Arc<RuntimeExternalRegistrar>>,
}

impl crate::SessionPlugin for RuntimeTestPlugin {
    fn id(&self) -> &'static str {
        "runtime-test"
    }

    fn register(&self, reg: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        if let Some(hook) = &self.before_turn {
            reg.turn().before(Arc::clone(hook));
        }
        if let Some(hook) = &self.checkpoint {
            reg.turn().checkpoint(Arc::clone(hook));
        }
        if let Some(projector) = &self.tool_result_projector {
            reg.tool_results().projector(Arc::clone(projector))?;
        }
        if let Some(hook) = &self.runtime_event {
            reg.session().on_event(Arc::clone(hook));
        }
        if let Some(register) = &self.external_registrar {
            register(reg)?;
        }
        Ok(())
    }
}

pub(crate) async fn runtime_with_plugins(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    transport: TestProvider,
) -> LashRuntime {
    runtime_with_plugins_and_tools_and_host(
        plugins,
        Arc::new(EmptyTools),
        transport,
        test_host_config(),
    )
    .await
}

pub(crate) async fn runtime_with_plugins_and_tools(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
) -> LashRuntime {
    runtime_with_plugins_and_tools_and_host(plugins, tools, transport, test_host_config()).await
}

pub(crate) async fn runtime_with_plugins_and_tools_and_host(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    let mut factories = plugins;
    let tools = Arc::clone(&tools);
    factories.push(Arc::new(StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
    )));
    let plugin_host = crate::PluginHost::new(factories);
    let plugin_session = plugin_host.build_session("root", None).expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        host,
        crate::RuntimeServices::new(plugin_session),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
    )
    .await
    .expect("runtime");
    runtime.host.process_registry = Some(Arc::new(crate::TestLocalProcessRegistry::default()));
    set_runtime_provider(&mut runtime, transport.clone().into_handle());
    runtime
}

pub(crate) async fn runtime_with_plugins_and_tools_and_host_and_store(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
    store: Arc<dyn crate::RuntimePersistence>,
) -> LashRuntime {
    let mut factories = plugins;
    let tools = Arc::clone(&tools);
    factories.push(Arc::new(StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
    )));
    let plugin_host = crate::PluginHost::new(factories);
    let plugin_session = plugin_host.build_session("root", None).expect("plugins");
    let services = crate::PersistentRuntimeServices::new(plugin_session, store);
    let mut runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        host,
        services,
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, transport.clone().into_handle());
    runtime
}

pub(crate) struct EchoTool;

fn echo_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
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
}

#[async_trait::async_trait]
impl crate::ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![echo_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(echo_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        assert_eq!(call.name, "echo_tool");
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        crate::ToolResult::ok(serde_json::json!({
            "payload": format!("raw:{value}")
        }))
    }
}

pub(crate) struct TerminalControlTool {
    pub(crate) controls: Vec<crate::ToolControl>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for TerminalControlTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        (0..self.controls.len())
            .map(|index| terminal_tool_definition(index).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        name.strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index < self.controls.len())
            .map(|index| Arc::new(terminal_tool_definition(index).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        self.result_for(call.name)
    }
}

impl TerminalControlTool {
    fn result_for(&self, name: &str) -> crate::ToolResult {
        let index = name
            .strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("known terminal test tool");
        crate::ToolResult::ok(serde_json::json!({ "tool": name }))
            .with_control(self.controls[index].clone())
    }
}

fn terminal_tool_definition(index: usize) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:terminal_tool_{index}"),
        format!("terminal_tool_{index}"),
        "Return a terminal control result",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

/// Tool that sleeps for 10 seconds unless its future is aborted or the
/// execution-context cancellation token fires. Used to verify that turn
/// cancellation unwinds in-flight tool tasks promptly.
pub(crate) struct SlowTool {
    pub(crate) observed_cancel: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for SlowTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![slow_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "slow_tool").then(|| Arc::new(slow_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let observed = Arc::clone(&self.observed_cancel);
        if let Some(token) = call.context.cancellation_token() {
            let token = token.clone();
            tokio::select! {
                _ = token.cancelled() => {
                    observed.store(true, Ordering::SeqCst);
                    crate::ToolResult::cancelled("cancelled")
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    crate::ToolResult::ok(serde_json::json!({"status": "completed"}))
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            crate::ToolResult::ok(serde_json::json!({"status": "completed"}))
        }
    }
}

fn slow_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:slow_tool",
        "slow_tool",
        "Sleep for a long time; respects cancellation.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

pub(crate) struct MemoryProbeTool;

#[async_trait::async_trait]
impl crate::ToolProvider for MemoryProbeTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![memory_probe_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "memory_probe").then(|| Arc::new(memory_probe_tool_definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
        crate::ToolResult::ok(json!("ok"))
    }
}

fn memory_probe_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:memory_probe",
        "memory_probe",
        "probe",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
}

pub(crate) struct ChildSessionTool;

#[async_trait::async_trait]
impl crate::ToolProvider for ChildSessionTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![child_session_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "spawn_child").then(|| Arc::new(child_session_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        let context = call.context;
        let child = match context
            .sessions()
            .create_session(
                crate::SessionCreateRequest::child_session(
                    context.session_id(),
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id("subagent-child")
                .with_plugin_source(crate::SessionPluginSource::CurrentSessionFork)
                .with_usage_source("subagent"),
            )
            .await
        {
            Ok(child) => child,
            Err(err) => return crate::ToolResult::err_fmt(format_args!("{err}")),
        };

        let turn = match context
            .sessions()
            .start_turn(
                &child.session_id,
                "subagent-child-turn",
                TurnInput {
                    items: vec![InputItem::Text {
                        text: "child turn".to_string(),
                    }],
                    protocol_turn_options: None,
                    trace_turn_id: None,
                    protocol_extension: None,
                    turn_context: crate::TurnContext::default(),
                },
            )
            .await
        {
            Ok(turn) => turn,
            Err(err) => return crate::ToolResult::err_fmt(format_args!("{err}")),
        };

        let _ = context.sessions().close_session(&child.session_id).await;
        let _ = turn;
        crate::ToolResult::ok(json!({ "status": "ok" }))
    }
}

fn child_session_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:spawn_child",
        "spawn_child",
        "spawn a child session",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

pub(crate) async fn standard_runtime_with_transport_and_host(
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(EmptyTools);
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        host,
        crate::RuntimeServices::new(plugin_session_with_tools("root", tools)),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
    )
    .await
    .expect("runtime");
    set_runtime_provider(&mut runtime, transport.clone().into_handle());
    runtime
}
