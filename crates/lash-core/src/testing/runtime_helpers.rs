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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use super::layered_backend::LayeredBackend;
pub use super::recording_store::{
    RecordingSessionStoreFactory, RecordingStore, SessionExecutionLeaseReleaseGate,
};

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

/// Admits `admitted` on a runtime host's own effect host.
///
/// A turn-driving test must bind its scope to the host the turn runs on: the
/// driver publishes the live opener to that host's `ToolChildHost` registry,
/// the execution context publishes recorded envs to that host's env store,
/// and group children resolve both through the executors registered on the
/// admitted controller — a scope minted on a foreign controller leaves every
/// child unroutable (ADR 0099 §2, §3).
/// `scoped_static` returns `None` for hosts that lend no `'static` controller
/// (a Restate `ctx`-bound one); there the caller's own bound controller is the
/// scope and this helper does not apply.
pub fn host_admitted_scope(
    config: &crate::RuntimeHostConfig,
    admitted: crate::AdmittedScope,
) -> crate::ScopedEffectController<'static> {
    config
        .control
        .effect_host
        .scoped_static(admitted)
        .expect("effect host scoped_static")
        .expect("effect host lends a 'static controller for this scope")
}

/// Admits `admitted` on `backend`'s effect host: the host a runtime built
/// over `backend` runs on, so its tool-child routing and env store are the
/// runtime's own.
pub fn backend_admitted_scope(
    backend: &Arc<dyn crate::Backend>,
    admitted: crate::AdmittedScope,
) -> crate::ScopedEffectController<'static> {
    backend
        .effect_host()
        .scoped_static(admitted)
        .expect("effect host scoped_static")
        .expect("effect host lends a 'static controller for this scope")
}

/// `backend_admitted_scope` for a turn scope.
pub fn backend_turn_scope(
    backend: &Arc<dyn crate::Backend>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    backend_admitted_scope(backend, crate::AdmittedScope::turn(session_id, turn_id))
}

/// `backend_admitted_scope` for a queued-work drain scope.
pub fn backend_queued_scope(
    backend: &Arc<dyn crate::Backend>,
    session_id: &SessionId,
    drain_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    backend_admitted_scope(
        backend,
        crate::AdmittedScope::queue_drain(session_id, drain_id.as_str()),
    )
}

/// `backend_admitted_scope` for the process scope a registry's first
/// registration mints (registration sequence 1), standing in for the worker's
/// admission step.
pub fn backend_process_scope(
    backend: &Arc<dyn crate::Backend>,
    process_id: impl Into<ProcessId>,
) -> crate::ScopedEffectController<'static> {
    backend_admitted_scope(
        backend,
        crate::AdmittedScope::process(crate::ProcessRef::new(
            process_id,
            crate::ProcessIncarnation::from_registration_sequence(1),
        )),
    )
}

/// `host_admitted_scope` for a turn scope.
pub fn host_turn_scope(
    config: &crate::RuntimeHostConfig,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    host_admitted_scope(config, crate::AdmittedScope::turn(session_id, turn_id))
}

/// `host_admitted_scope` for a queued-work drain scope.
pub fn host_queued_scope(
    config: &crate::RuntimeHostConfig,
    session_id: &SessionId,
    drain_id: &TurnId,
) -> crate::ScopedEffectController<'static> {
    host_admitted_scope(
        config,
        crate::AdmittedScope::queue_drain(session_id, drain_id.as_str()),
    )
}

/// `host_admitted_scope` for the process scope a test registry's first
/// registration mints (registration sequence 1), standing in for the worker's
/// admission step.
pub fn host_process_scope(
    config: &crate::RuntimeHostConfig,
    process_id: impl Into<ProcessId>,
) -> crate::ScopedEffectController<'static> {
    host_admitted_scope(
        config,
        crate::AdmittedScope::process(crate::ProcessRef::new(
            process_id,
            crate::ProcessIncarnation::from_registration_sequence(1),
        )),
    )
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

/// A host over `backend` whose provider resolver answers with an empty mock
/// provider.
pub fn test_host_config(backend: &Arc<dyn crate::Backend>) -> EmbeddedRuntimeHost {
    let mut config = test_runtime_host_config(backend);
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
}

pub fn test_host_config_with_trace_path(
    backend: &Arc<dyn crate::Backend>,
    path: PathBuf,
) -> EmbeddedRuntimeHost {
    let mut config = test_runtime_host_config(backend);
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    EmbeddedRuntimeHost::new(config)
}

pub fn test_host_config_with_trace_path_and_stream_events(
    backend: &Arc<dyn crate::Backend>,
    path: PathBuf,
) -> EmbeddedRuntimeHost {
    let mut config = test_runtime_host_config(backend);
    config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path)));
    config.tracing.trace_level = lash_trace::TraceLevel::Extended;
    EmbeddedRuntimeHost::new(config)
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

/// Advance `store`'s durable head the way another writer would: load the
/// persisted session (or start from an empty one when the bound session has no
/// head yet), apply `change`, and commit it as the next revision.
/// Returns the head that commit wrote.
pub async fn advance_session_head(
    store: &dyn crate::RuntimePersistence,
    usage_deltas: &[crate::TokenLedgerEntry],
    change: impl FnOnce(&mut RuntimeSessionState),
) -> crate::SessionHeadMeta {
    let persisted = crate::store::load_persisted_session_state(store)
        .await
        .expect("load the persisted session");
    let mut state = match persisted {
        Some(state) => state,
        // A session with no committed head: the other writer commits its
        // first one.
        None => {
            let meta = store
                .load_session_meta()
                .await
                .expect("load the session binding")
                .expect("the store is bound to a session");
            let mut state =
                RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
            state.session_id = meta.session_id;
            state
        }
    };
    change(&mut state);
    store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(
            &state,
            usage_deltas,
        ))
        .await
        .expect("commit the advanced head");
    store
        .load_session_head_meta()
        .await
        .expect("read the advanced head")
        .expect("the advanced head exists")
}

/// A fresh root session store for `session_id` from `backend`'s catalog,
/// under a [`RecordingStore`].
pub async fn recording_session_store(
    backend: &Arc<dyn crate::Backend>,
    session_id: impl Into<SessionId>,
) -> Arc<RecordingStore> {
    let store = backend
        .session_store_factory()
        .create_store(&crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.into(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create a session store from the backend catalog");
    Arc::new(RecordingStore::over(store))
}

pub fn test_commit_budget() -> crate::CommitBudget {
    crate::CommitBudget::bounded(1024 * 1024, 512)
}

/// A host config over `backend` with the test commit budget and a batching
/// bound of one.
pub fn test_runtime_host_config(backend: &Arc<dyn crate::Backend>) -> RuntimeHostConfig {
    RuntimeHostConfig::new(
        Arc::clone(backend),
        test_commit_budget(),
        crate::QueuedWorkBatchingConfig::new(1),
    )
}

pub fn test_runtime_host_config_with_provider(
    backend: &Arc<dyn crate::Backend>,
    provider: crate::ProviderHandle,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config(backend);
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
    /// A runtime over `backend`: its host, and its process registry unless
    /// [`Self::without_process_registry`] drops it.
    pub fn new(backend: &Arc<dyn crate::Backend>, transport: TestProvider) -> Self {
        Self {
            attachment_acceptance: Default::default(),
            plugins: crate::testing::test_standard_protocol_factories(),
            tools: Arc::new(EmptyTools),
            transport,
            host: test_host_config(backend),
            store: None,
            process_registry: Some(backend.process_registry()),
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

    /// Replace the host. Its backend's process registry replaces the
    /// runtime's unless the runtime runs without one.
    pub fn host(mut self, host: EmbeddedRuntimeHost) -> Self {
        if self.process_registry.is_some() {
            self.process_registry = Some(host.core.backend().process_registry());
        }
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
        let attachment_store = Arc::clone(&self.host.core.durability.attachment_store);
        let process_env_store = Arc::clone(&self.host.core.durability.process_env_store);
        let runtime = match (self.store, self.process_registry) {
            (Some(store), None) => LashRuntime::from_persistent_embedded_state(
                policy,
                self.host,
                crate::PersistentRuntimeServices::new(
                    plugin_session,
                    store,
                    attachment_store,
                    process_env_store,
                ),
                initial_state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
            .expect("runtime"),
            (None, None) => LashRuntime::from_embedded_state(
                policy,
                self.host,
                crate::RuntimeServices::new(plugin_session, attachment_store, process_env_store),
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
                    crate::PersistentRuntimeServices::new(
                        plugin_session,
                        store,
                        attachment_store,
                        process_env_store,
                    ),
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
                    crate::RuntimeServices::new(
                        plugin_session,
                        attachment_store,
                        process_env_store,
                    ),
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

pub async fn standard_runtime_with_transport(
    backend: &Arc<dyn crate::Backend>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport).build().await
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
    pub presentation_steps: Vec<crate::plugin::ToolPresentationStep>,
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
        for step in &self.presentation_steps {
            reg.tool_results().presentation_step(Arc::clone(step));
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
    backend: &Arc<dyn crate::Backend>,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport)
        .plugins(plugins)
        .build()
        .await
}

pub async fn runtime_with_plugins_and_tools(
    backend: &Arc<dyn crate::Backend>,
    plugins: Vec<Arc<dyn crate::PluginFactory>>,
    tools: Arc<dyn crate::ToolProvider>,
    transport: TestProvider,
) -> LashRuntime {
    TestRuntime::new(backend, transport)
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
    let backend = Arc::clone(host.core.backend());
    TestRuntime::new(&backend, transport)
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
    let backend = Arc::clone(host.core.backend());
    TestRuntime::new(&backend, transport)
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

pub async fn standard_runtime_with_transport_and_host(
    transport: TestProvider,
    host: EmbeddedRuntimeHost,
) -> LashRuntime {
    let backend = Arc::clone(host.core.backend());
    TestRuntime::new(&backend, transport)
        .host(host)
        .build()
        .await
}

/// Reopen a session that session initialisation committed, through the
/// ordinary resumed-open path — the same open any embedder performs on
/// durable state. The returned runtime is an ordinary session owned by the
/// caller; nothing registers it anywhere (FIG-3378).
pub fn reopen_session_runtime<'a>(
    parent: &'a LashRuntime,
    session_id: &'a SessionId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = LashRuntime> + Send + 'a>> {
    Box::pin(async move {
        let factory = parent.host.core.session_store_factory();
        let store = factory
            .open_existing_store_by_id(session_id)
            .await
            .expect("open child session store")
            .expect("child session store exists");
        let state = crate::store::load_persisted_session_state(store.as_ref())
            .await
            .expect("load child session state")
            .expect("persisted child session state");
        let policy = state.effective_policy().clone();
        let is_root = state.authority.subagent.is_none();
        let plugin_host = parent
            .session
            .as_ref()
            .expect("parent session")
            .plugins()
            .host()
            .clone();
        let mut env = crate::RuntimeEnvironment::builder(parent.host.core.clone())
            .with_plugin_host(Arc::new(plugin_host))
            .build();
        env.work = parent.host.work.clone();
        let mut child = LashRuntime::from_environment(
            &env,
            policy,
            state,
            Some(store),
            parent.runtime_lease_owner.clone(),
        )
        .await
        .expect("reopen child session runtime");
        child
            .configure_protocol_on_materialize(&crate::PluginOptions::default(), is_root)
            .expect("materialize reopened child protocol configuration");
        child
    })
}
