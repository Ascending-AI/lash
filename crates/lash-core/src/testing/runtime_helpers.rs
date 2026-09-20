//! Runtime turn-machine fixtures shared by `lash-core`'s own unit tests and by
//! the relocated `runtime::tests` integration binaries.
//!
//! These were `crate::runtime::tests::helpers` until the runtime suites were
//! promoted out of the library's unit-test target; they live behind the
//! `testing` feature, the crate's existing seam for test-only surface.

use crate::llm::transport::LlmTransportError;
use crate::llm::types::LlmStreamEvent;
use crate::plugin::StaticPluginFactory;
use crate::runtime::*;
use crate::testing::TestProvider;
use crate::*;
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// The public in-memory store is the single in-memory `RuntimePersistence` impl;
// tests use it under the historical `RecordingStore` name (its `pub`
// fields + recording-count getters back the existing assertions).
pub use crate::runtime::InMemorySessionStore as RecordingStore;

pub struct FixedAttachmentRoots(pub std::collections::BTreeSet<crate::AttachmentId>);

#[async_trait::async_trait]
#[diagnostic::do_not_recommend]
impl crate::AttachmentRootSet for FixedAttachmentRoots {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Ok(self.0.clone())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Ok(self.0.contains(id))
    }
}

pub fn default_state() -> RuntimeSessionState {
    let mut state =
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.ensure_agent_frame_initialized();
    state
}

pub fn native_scope(scope: crate::ExecutionScope) -> crate::ScopedEffectController<'static> {
    crate::ScopedEffectController::shared(
        Arc::new(crate::NativeRuntimeEffectController::default()),
        scope,
    )
    .expect("native execution scope")
}

pub fn named_turn_scope(
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    native_scope(crate::ExecutionScope::turn(session_id, turn_id))
}

pub trait ReadModelState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel;
}

impl ReadModelState for SessionSnapshot {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
            .expect("test snapshot frame scope resolves")
    }
}

impl ReadModelState for RuntimeSessionState {
    fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.read_model()
            .expect("test runtime frame scope resolves")
    }
}

pub trait ReadModelStateMut: ReadModelState {
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

pub fn active_conversation_messages(state: &impl ReadModelState) -> Vec<Message> {
    state.read_model().messages.as_ref().clone()
}

pub fn append_message(state: &mut impl ReadModelStateMut, message: Message) {
    state.append_message(message);
}

#[derive(Clone, Default)]
pub struct RecordingSink {
    pub events: Arc<Mutex<Vec<SessionStreamEvent>>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: SessionStreamEvent) {
        self.events.lock_recover().push(event);
    }
}

impl RecordingSink {
    pub fn snapshot(&self) -> Vec<SessionStreamEvent> {
        self.events.lock_recover().clone()
    }
}

#[derive(Clone, Default)]
pub struct RecordingTurnEvents {
    pub events: Arc<Mutex<Vec<TurnActivity>>>,
}

#[async_trait::async_trait]
impl TurnActivitySink for RecordingTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock_recover().push(activity);
    }
}

impl RecordingTurnEvents {
    pub fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock_recover().clone()
    }
}

#[derive(Debug)]
pub struct MockCall {
    pub stream_events: Vec<LlmStreamEvent>,
    pub response: Result<LlmResponse, LlmTransportError>,
}

pub fn mock_provider(calls: Vec<MockCall>) -> TestProvider {
    mock_provider_with_kind("mock", calls)
}

pub fn mock_openai_compatible_provider(calls: Vec<MockCall>) -> TestProvider {
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

pub fn set_runtime_provider(runtime: &mut LashRuntime, provider: crate::ProviderHandle) {
    runtime.host.core.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider.clone()));
    runtime.state.policy.provider_id = provider.kind().to_string();
}

pub use crate::testing::standard_test_policy;

pub fn test_host_config() -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
        .with_session_store_factory(Arc::new(crate::InMemorySessionStoreFactory::new()))
}

pub fn test_host_config_with_trace_path(path: PathBuf) -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    EmbeddedRuntimeHost::new(config)
}

pub fn test_host_config_with_trace_path_and_stream_events(path: PathBuf) -> EmbeddedRuntimeHost {
    let mut config = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    config.tracing.trace_level = lash_trace::TraceLevel::Extended;
    EmbeddedRuntimeHost::new(config)
}

#[derive(Clone, Default)]
pub struct RecordingSessionStoreFactory {
    stores: Arc<StdMutex<Vec<Arc<RecordingStore>>>>,
    defer_metadata_to_admission: bool,
}

impl RecordingSessionStoreFactory {
    pub fn stores(&self) -> Vec<Arc<RecordingStore>> {
        self.stores.lock_recover().clone()
    }

    pub fn deferring_metadata_to_admission(mut self) -> Self {
        self.defer_metadata_to_admission = true;
        self
    }
}

// RecordingSessionStoreFactory retains every attachment-aware store it creates,
// so its root-set answer is the union of those stores' manifests.
#[async_trait::async_trait]
#[diagnostic::do_not_recommend]
impl crate::AttachmentRootSet for RecordingSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
        let mut refs = std::collections::BTreeSet::new();
        for store in self.stores() {
            crate::AttachmentManifest::forget_aged_uncommitted_intents(
                &*store,
                intent_grace_cutoff_epoch_ms,
            )
            .await?;
            refs.extend(crate::AttachmentManifest::list_all_refs(&*store).await?);
        }
        Ok(refs)
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        for store in self.stores() {
            if crate::AttachmentManifest::has_live_ref_for_id(
                &*store,
                id,
                intent_grace_cutoff_epoch_ms,
            )
            .await?
            {
                return Ok(true);
            }
        }
        Ok(false)
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
                pending_observer_intents: Vec::new(),
                session_id: request.session_id.clone(),
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

    // The recorded stores are the catalog, so a by-id lookup is the same scan
    // the request-shaped seam performs.
    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
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
                    .is_some_and(|meta| meta.session_id == *session_id)
            })
            .cloned()
            .map(|store| store as Arc<dyn crate::store::RuntimePersistence>))
    }

    // Recorded stores are retained, never tombstoned: this fixture drops no
    // session, so no id has a deletion marker.
    async fn session_was_deleted(&self, _session_id: &SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        Ok(crate::store::SessionBlobReclaimReport::default())
    }
}

pub fn plugin_session_with_orchestrating_tool(
    session_id: &SessionId,
    tool: crate::tool_provider::orchestration::OrchestratingToolDef,
) -> Arc<crate::PluginSession> {
    let tool_factory = StaticPluginFactory::new(
        "test_orchestrating_tools",
        crate::PluginSpec::new().with_orchestrating_tool(tool),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(tool_factory));
    crate::PluginHost::new(factories)
        .build_session(session_id)
        .expect("plugins")
}

pub fn plugin_session_with_tools(
    session_id: &SessionId,
    tools: Arc<dyn crate::ToolProvider>,
) -> Arc<crate::PluginSession> {
    let tool_factory = StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
    );
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(tool_factory));
    crate::PluginHost::new(factories)
        .build_session(session_id)
        .expect("plugins")
}

pub struct EmptyTools;

pub fn test_commit_budget() -> crate::CommitBudget {
    crate::CommitBudget::bounded(1024 * 1024, 512)
}

pub fn test_runtime_host_config() -> RuntimeHostConfig {
    RuntimeHostConfig::in_memory(
        test_commit_budget(),
        crate::QueuedWorkBatchingConfig::new(1),
    )
}

pub fn test_runtime_host_config_with_provider(
    provider: crate::ProviderHandle,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config();
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
    config
}

#[async_trait::async_trait]
impl crate::ToolProvider for EmptyTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        None
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err(serde_json::json!("Unknown tool")).into()
    }
}

pub struct TestRuntime {
    attachment_acceptance: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    session_id: Option<SessionId>,
}

impl TestRuntime {
    pub fn new(transport: TestProvider) -> Self {
        Self {
            attachment_acceptance: Default::default(),
            plugins: crate::testing::test_standard_protocol_factories(),
            tools: Arc::new(EmptyTools),
            transport,
            host: test_host_config(),
            store: None,
            process_registry: Some(Arc::new(crate::TestLocalProcessRegistry::default())),
            session_id: None,
        }
    }

    pub fn attachment_acceptance(
        mut self,
        snapshot: Arc<crate::provider::AttachmentCapabilitySnapshot>,
    ) -> Self {
        self.attachment_acceptance = snapshot;
        self
    }

    pub fn plugins(mut self, plugins: Vec<Arc<dyn crate::PluginFactory>>) -> Self {
        self.plugins = plugins;
        self
    }

    pub fn tools(mut self, tools: Arc<dyn crate::ToolProvider>) -> Self {
        self.tools = tools;
        self
    }

    pub fn host(mut self, host: EmbeddedRuntimeHost) -> Self {
        self.host = host;
        self
    }

    pub fn store(mut self, store: Arc<dyn crate::RuntimePersistence>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn process_registry(mut self, process_registry: Arc<dyn crate::ProcessRegistry>) -> Self {
        self.process_registry = Some(process_registry);
        self
    }

    pub fn without_process_registry(mut self) -> Self {
        self.process_registry = None;
        self
    }

    pub async fn build(self) -> LashRuntime {
        // `lash-core`'s own `cfg(test)` binary used to get the in-tree protocol
        // fake injected by `builtin_plugin_factories`. The relocated runtime
        // suites link the library without `cfg(test)`, so reproduce that
        // injection here, with the same id-override rule `PluginHost::new`
        // applies, rather than widening the builtin set for every crate that
        // turns on the `testing` feature.
        let mut factories = self.plugins;
        let tools = Arc::clone(&self.tools);
        factories.push(Arc::new(StaticPluginFactory::new(
            "test_tools",
            crate::PluginSpec::new().with_tool_provider(Arc::clone(&tools)),
        )));
        let plugin_host = crate::testing::test_plugin_host(factories);
        let plugin_session = plugin_host.build_session("root").expect("plugins");
        let mut initial_state =
            RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
        if let Some(session_id) = self.session_id {
            initial_state.session_id = session_id.clone();
            initial_state.policy.session_id = Some(session_id);
        }
        let mut policy = standard_test_policy();
        policy.model.capability.attachment_acceptance = self.attachment_acceptance.clone();
        initial_state.policy.model.capability.attachment_acceptance = self.attachment_acceptance;
        let runtime = match (self.store, self.process_registry) {
            (Some(store), None) => LashRuntime::from_persistent_embedded_state(
                policy,
                self.host,
                crate::PersistentRuntimeServices::new(plugin_session, store),
                initial_state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
            .expect("runtime"),
            (None, None) => LashRuntime::from_embedded_state(
                policy,
                self.host,
                crate::RuntimeServices::new(plugin_session),
                initial_state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
            .expect("runtime"),
            (Some(store), Some(registry)) => {
                let host = crate::ProcessRuntimeHost::with_ports(
                    self.host,
                    crate::testing::process_work_wiring_for_registry(registry),
                    Arc::new(crate::NoQueuedWork::new()),
                );
                LashRuntime::from_persistent_background_state(
                    policy,
                    host,
                    crate::PersistentRuntimeServices::new(plugin_session, store),
                    initial_state.clone(),
                    crate::testing::runtime_lease_owner(),
                )
                .await
                .expect("runtime")
            }
            (None, Some(registry)) => {
                let host = crate::ProcessRuntimeHost::with_ports(
                    self.host,
                    crate::testing::process_work_wiring_for_registry(registry),
                    Arc::new(crate::NoQueuedWork::new()),
                );
                LashRuntime::from_background_state(
                    policy,
                    host,
                    crate::RuntimeServices::new(plugin_session),
                    initial_state,
                    crate::testing::runtime_lease_owner(),
                )
                .await
                .expect("runtime")
            }
        };
        let mut runtime = runtime;
        set_runtime_provider(&mut runtime, self.transport.into_handle());
        runtime
    }
}

pub async fn standard_runtime_with_transport(transport: TestProvider) -> LashRuntime {
    TestRuntime::new(transport).build().await
}
pub type RuntimeTestPluginBuilder = dyn Fn(&crate::PluginSessionContext) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError>
    + Send
    + Sync;
pub type RuntimeExternalRegistrar =
    dyn Fn(&mut crate::PluginRegistrar) -> Result<(), crate::PluginError> + Send + Sync;

pub struct RuntimeTestPluginFactory {
    pub build: Arc<RuntimeTestPluginBuilder>,
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

pub struct RuntimeTestPlugin {
    pub before_turn: Option<crate::plugin::BeforeTurnHook>,
    pub checkpoint: Option<crate::plugin::CheckpointHook>,
    pub tool_result_projector: Option<crate::plugin::ToolResultProjector>,
    pub runtime_event: Option<crate::plugin::PluginLifecycleEventHook>,
    pub external_registrar: Option<Arc<RuntimeExternalRegistrar>>,
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

pub async fn runtime_with_plugins(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(transport).plugins(plugins).build().await
}

pub async fn runtime_with_plugins_and_tools(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(transport)
        .plugins(plugins)
        .tools(tools)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools_and_host(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    TestRuntime::new(transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools_and_host_and_store(
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
    store: Arc<dyn crate::RuntimePersistence>,
) -> LashRuntime {
    TestRuntime::new(transport)
        .plugins(plugins)
        .tools(tools)
        .host(host)
        .store(store)
        .build()
        .await
}

pub struct EchoTool;

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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        assert_eq!(call.name(), "echo_tool");
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        crate::ToolOutcome::ok(serde_json::json!({
            "payload": format!("raw:{value}")
        }))
        .into()
    }
}

pub struct TerminalControlTool {
    pub controls: Vec<crate::ToolControl>,
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

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.result_for(call.name()).into()
    }
}

impl TerminalControlTool {
    fn result_for(&self, name: &str) -> crate::ToolOutcome {
        let index = name
            .strip_prefix("terminal_tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .expect("known terminal test tool");
        crate::ToolOutcome::ok(serde_json::json!({ "tool": name }))
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
pub struct SlowTool {
    pub observed_cancel: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for SlowTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![slow_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "slow_tool").then(|| Arc::new(slow_tool_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let observed = Arc::clone(&self.observed_cancel);
        if let Some(token) = call.context.cancellation_token() {
            let token = token.clone();
            tokio::select! {
                _ = token.cancelled() => {
                    observed.store(true, Ordering::SeqCst);
                    crate::ToolOutcome::cancelled("cancelled").into()
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    crate::ToolOutcome::ok(serde_json::json!({"status": "completed"})).into()
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            crate::ToolOutcome::ok(serde_json::json!({"status": "completed"})).into()
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

pub struct MemoryProbeTool;

#[async_trait::async_trait]
impl crate::ToolProvider for MemoryProbeTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![memory_probe_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "memory_probe").then(|| Arc::new(memory_probe_tool_definition().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(json!("ok")).into()
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

pub struct ChildSessionTool;

impl ChildSessionTool {
    /// Managed child turns are journal-capable session work, so this test tool
    /// registers in the runtime-owned orchestrating lane; a recorded leaf
    /// attempt has no route to `sessions().start_turn()`.
    #[expect(
        unsafe_code,
        reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
    )]
    pub fn orchestrating() -> crate::tool_provider::orchestration::OrchestratingToolDef {
        let implementation: Arc<
            dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
        > = Arc::new(Self);
        // SAFETY: lash-core owns this test-only tool contract and its body.
        unsafe {
            crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(
                implementation,
            )
        }
    }
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation for ChildSessionTool {
    fn manifest(&self) -> crate::ToolManifest {
        child_session_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(child_session_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
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
            Err(err) => return crate::ToolOutcome::err_fmt(format_args!("{err}")),
        };

        let turn = match context
            .sessions()
            .start_turn(
                &child.session_id,
                &TurnId::from("subagent-child-turn"),
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
            Err(err) => return crate::ToolOutcome::err_fmt(format_args!("{err}")),
        };

        let _ = context.sessions().close_session(&child.session_id).await;
        let _ = turn;
        crate::ToolOutcome::ok(json!({ "status": "ok" }))
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

pub async fn standard_runtime_with_transport_and_host(
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    TestRuntime::new(transport).host(host).build().await
}
