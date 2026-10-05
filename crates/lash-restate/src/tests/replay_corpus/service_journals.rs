//! The journals lash's own Restate services write, recorded from the real
//! handlers on the in-process server double (FIG-4805).
//!
//! One workload runs a session turn with a native tool call and starts a
//! process through the deployment's bound services; `run_scenarios` adds
//! one workload per recorded Run behaviour. A service's journal is the
//! ordered commands its handlers wrote; a `RunCommand` whose run settled a
//! Run record carries that record's families in brackets, and notifications
//! — which the server stores in arrival order — are paired back to their
//! command by completion id rather than rendered.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::services::LASH_SERVICES;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    HandlerAttempt, InvocationView, RestateTestBackend, RestateTestServer, ServerConfig,
};
use serde_json::json;

const SEED: u64 = 0x4805;
const TOOL: &str = "count_call";
const ENGINE_KIND: &str = "fig-4805-replay-corpus";
/// The plugin the corpus composition adds over the base tool: scripted Run
/// behaviours (`pair_call`, `retry_once`, `gate_call`, `state_add`) plus the
/// `increment` reducer `state_add` publishes under.
pub(super) const TOOLS_PLUGIN: &str = "replay-corpus-tools";
const SESSION: &str = "replay-corpus-session";
const TURN: &str = "replay-corpus-turn";
const START_TURN: &str = "replay-corpus-start";
pub(super) const BOUND: Duration = Duration::from_secs(60);

/// A tool that answers once the workload releases it.
struct CountingTool {
    release: Arc<tokio::sync::Semaphore>,
    awaited_child: bool,
    tools: Option<Arc<CorpusTools>>,
}

/// The tool's definition: one that awaits its declared child declares it may
/// defer.
fn tool_definition(awaited_child: bool) -> lash_core::ToolDefinition {
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
    .expect("valid declared tool schemas");
    if awaited_child {
        definition.with_declaration(lash_core::ToolDeclaration::deferring())
    } else {
        definition.with_declaration(
            lash_core::ToolDeclaration::default()
                .with_intents([lash_core::ToolIntentKind::StartProcess]),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition(self.awaited_child).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition(self.awaited_child).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Some(tools) = &self.tools {
            tools.record(&call);
        }
        self.release
            .acquire()
            .await
            .expect("the workload keeps the tool's gate open")
            .forget();
        let declaration = lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::Engine {
                kind: ENGINE_KIND.into(),
                payload: json!({}),
            },
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(
            call.context
                .process_execution_env_ref()
                .expect("the admitted tool carries its execution environment"),
        );
        let intent = lash_core::StartProcessIntent {
            owner: call.context.owner().runtime_owner(),
            declaration,
        };
        if self.awaited_child {
            let start = lash_core::DeclaredStart::new(call.context, intent)
                .expect("the tool may await its declared child");
            return lash_core::ToolAttemptOutcome::pending(
                lash_core::PendingCompletion::new().resolved_by_declared_start(start),
            );
        }
        // A recorded final with a real intent exercises LashToolRealization's
        // independent journal as part of the existing service workload.
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(json!({"result": "counted"})),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::StartProcess(Box::new(intent))]),
        )
    }
}

/// A process engine whose run settles at once.
struct SettlingEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for SettlingEngine {
    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        unreachable!("the settling engine stores no artifacts")
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(json!({"settled": true})),
            )),
            prelude: Vec::new(),
        })
    }
}

struct EnginePluginFactory;

impl lash::plugins::PluginFactory for EnginePluginFactory {
    fn id(&self) -> &'static str {
        ENGINE_KIND
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::new(SettlingEngine) as Arc<dyn lash_core::ProcessEngine>,
        )])
    }

    fn build(
        &self,
        _context: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(EngineSessionPlugin))
    }
}

struct EngineSessionPlugin;

impl lash::plugins::SessionPlugin for EngineSessionPlugin {
    fn id(&self) -> &'static str {
        ENGINE_KIND
    }

    fn register(
        &self,
        _registrar: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

/// One tool body's out-of-journal delivery evidence: the corpus's semantic
/// oracle — which call, which attempt — where the journal only holds names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BodyDelivery {
    pub tool: String,
    pub label: Option<String>,
    pub call_id: String,
    pub attempt: u32,
}

/// Shared state the `replay-corpus-tools` plugin's bodies coordinate
/// through: a delivery log, a wake on every entry, and the `pair_call` "b"
/// gate a scenario opens once it has armed its crash.
#[derive(Default)]
pub(super) struct CorpusTools {
    deliveries: Mutex<Vec<BodyDelivery>>,
    entered: tokio::sync::Notify,
    pair_gate_open: AtomicBool,
    pair_gate: tokio::sync::Notify,
    gate_call_started: AtomicBool,
}

impl CorpusTools {
    /// Every body delivery so far, in entry order.
    pub(super) fn deliveries(&self) -> Vec<BodyDelivery> {
        self.deliveries.lock().expect("deliveries lock").clone()
    }

    /// Resolves once a delivery satisfying `ready` is recorded.
    pub(super) async fn wait_for_delivery(&self, ready: impl Fn(&BodyDelivery) -> bool) {
        tokio::time::timeout(BOUND, async {
            loop {
                if self
                    .deliveries
                    .lock()
                    .expect("deliveries lock")
                    .iter()
                    .any(&ready)
                {
                    return;
                }
                self.entered.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("a body never delivered: {:?}", self.deliveries()));
    }

    /// Let `pair_call` "b" bodies finish — the current one and its replay.
    pub(super) fn open_pair_gate(&self) {
        self.pair_gate_open.store(true, Ordering::SeqCst);
        self.pair_gate.notify_waiters();
    }

    fn record(&self, call: &lash_core::ToolCall<'_>) -> BodyDelivery {
        let delivery = BodyDelivery {
            tool: call.name().to_string(),
            label: call.args["label"].as_str().map(str::to_owned),
            call_id: call.context.call_id().to_string(),
            attempt: call.context.attempt_number(),
        };
        self.deliveries
            .lock()
            .expect("deliveries lock")
            .push(delivery.clone());
        self.entered.notify_waiters();
        delivery
    }
}

/// The plugin itself: factory, session registration and body executor, all
/// over the one `CorpusTools` the scenario keeps a handle on.
#[derive(Clone)]
pub(super) struct CorpusToolsPlugin {
    tools: Arc<CorpusTools>,
}

impl CorpusToolsPlugin {
    pub(super) fn new(tools: Arc<CorpusTools>) -> Self {
        Self { tools }
    }
}

impl lash::plugins::PluginFactory for CorpusToolsPlugin {
    fn id(&self) -> &'static str {
        TOOLS_PLUGIN
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn build(
        &self,
        _context: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for CorpusToolsPlugin {
    fn id(&self) -> &'static str {
        TOOLS_PLUGIN
    }

    fn register(
        &self,
        registrar: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        registrar.state_reducer(
            "increment",
            Arc::new(|reduction: lash::plugins::StateReduction<'_>| {
                let current = reduction
                    .current
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Ok(Some(json!(current + 1)))
            }),
        )?;
        let tool = |name: &str, declaration: Option<lash_core::ToolDeclaration>| {
            let mut definition = lash_core::ToolDefinition::raw(
                format!("tool:{name}"),
                name,
                "Replay-corpus scripted tool.",
                json!({
                    "type": "object",
                    "properties": { "label": { "type": "string" } },
                    "additionalProperties": false,
                }),
                json!({ "type": "object" }),
            )
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
            if let Some(declaration) = declaration {
                definition = definition.with_declaration(declaration);
            }
            if name == "retry_once" {
                definition =
                    definition.with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 10, 10));
            }
            Ok(definition)
        };
        let definitions = [
            tool("pair_call", None),
            tool("retry_once", None),
            tool("gate_call", None),
            tool("state_add", None),
        ]
        .into_iter()
        .collect::<Result<Vec<_>, lash_core::PluginError>>()?;
        registrar
            .tools()
            .provider(Arc::new(lash::tools::StaticToolProvider::new(
                definitions,
                self.clone(),
            )))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for CorpusToolsPlugin {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let delivery = self.tools.record(&call);
        match call.name() {
            "pair_call" => {
                if delivery.label.as_deref() == Some("b") {
                    while !self.tools.pair_gate_open.load(Ordering::SeqCst) {
                        self.tools.pair_gate.notified().await;
                    }
                }
                lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
            }
            "retry_once" if delivery.attempt == 1 => lash_core::ToolOutcome::retryable_failure(
                lash_core::ToolFailureClass::External,
                "replay-corpus-retry",
                "first attempt reports a retryable failure",
                Some(10),
            )
            .into(),
            "retry_once" => lash_core::ToolOutcome::ok(json!({"result": "counted"})).into(),
            // The first body of a run wedges until its run is cancelled; every
            // later body returns at once.
            "gate_call" if !self.tools.gate_call_started.swap(true, Ordering::SeqCst) => {
                std::future::pending::<lash_core::ToolAttemptOutcome>().await
            }
            "gate_call" => lash_core::ToolOutcome::ok(json!({"result": "counted"})).into(),
            "state_add" => lash_core::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::ok(json!({"result": "counted"})).with_state(
                    lash::plugins::StateCommands::new().apply(
                        "total",
                        "increment",
                        serde_json::Value::Null,
                    ),
                ),
            ),
            other => {
                lash_core::ToolOutcome::err_fmt(format_args!("unknown replay-corpus tool {other}"))
                    .into()
            }
        }
    }
}

fn llm_profile_spec() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// The user input text of the request's current segment.
fn user_text(request: &LlmRequest) -> String {
    let text_of = |message: &lash_core::llm::types::LlmMessage| {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                lash_core::llm::types::LlmContentBlock::Text { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    request
        .messages
        .iter()
        .rfind(|message| message.starts_user_segment)
        .map(|message| text_of(message).join("\n"))
        .unwrap_or_else(|| {
            request
                .messages
                .iter()
                .flat_map(text_of)
                .collect::<Vec<_>>()
                .join("\n")
        })
}

/// A stateless scripted model: it runs the tool round the current user
/// segment's text, then answers once that segment holds a tool result.
fn model_reply(request: &LlmRequest) -> LlmResponse {
    let last_segment = request
        .messages
        .iter()
        .rposition(|message| message.starts_user_segment)
        .map(|index| &request.messages[index..])
        .unwrap_or(&request.messages[..]);
    let saw_tool_result = last_segment.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                lash_core::llm::types::LlmContentBlock::ToolResult { .. }
            )
        })
    });
    let tool_call =
        |call_id: &str, tool_name: &str, input: serde_json::Value| LlmOutputPart::ToolCall {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            input_json: input.to_string(),
            replay: None,
        };
    let parts = if saw_tool_result {
        vec![LlmOutputPart::Text {
            text: "answered".to_owned(),
            response_meta: None,
        }]
    } else {
        match user_text(request) {
            text if text.contains("pair") => vec![
                tool_call("call-a", "pair_call", json!({"label": "a"})),
                tool_call("call-b", "pair_call", json!({"label": "b"})),
            ],
            text if text.contains("retry") => {
                vec![tool_call("call-1", "retry_once", json!({}))]
            }
            text if text.contains("cancel") => {
                vec![tool_call("call-1", "gate_call", json!({}))]
            }
            text if text.contains("state") => {
                vec![tool_call("call-1", "state_add", json!({}))]
            }
            _ => vec![tool_call("call-1", TOOL, json!({}))],
        }
    };
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

pub(super) fn build_core(
    backend: lash_core::Backend,
    release: &Arc<tokio::sync::Semaphore>,
    cancel_watch: Option<RestateTestServer>,
) -> lash::LashCore {
    build_core_with_tools(
        backend,
        release,
        None,
        None,
        Arc::new(CorpusTools::default()),
        cancel_watch,
    )
}

pub(in crate::tests) fn build_core_with_trace(
    backend: lash_core::Backend,
    release: &Arc<tokio::sync::Semaphore>,
    tracing: Option<lash_core::facade_support::TraceRuntime>,
    calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
) -> lash::LashCore {
    build_core_with_tools(
        backend,
        release,
        tracing,
        calls,
        Arc::new(CorpusTools::default()),
        None,
    )
}

/// The corpus composition: [`build_core_with_trace`] plus the
/// `replay-corpus-tools` plugin over a caller-owned `CorpusTools`.
///
/// With `cancel_watch`, a model call answers only once some turn's
/// cancellation-gate watch has reached its `await_resolution` waiter on that
/// server. Each model call watches the gate for its own lifetime, and a
/// scripted answer would otherwise race the watch's attach: whether the
/// recording holds the waiter and its index `register`/`settle` would depend
/// on scheduling.
pub(super) fn build_core_with_tools(
    backend: lash_core::Backend,
    release: &Arc<tokio::sync::Semaphore>,
    tracing: Option<lash_core::facade_support::TraceRuntime>,
    calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
    tools: Arc<CorpusTools>,
    cancel_watch: Option<RestateTestServer>,
) -> lash::LashCore {
    let awaited_child = calls.is_some();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("replay-corpus")
        .complete(move |request: LlmRequest| {
            let mut reply = model_reply(&request);
            if let Some(calls) = &calls {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                reply.usage = lash_core::llm::types::LlmUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    ..Default::default()
                };
                reply.provider_usage = Some(json!({"input_tokens": 10, "output_tokens": 4}));
            }
            let cancel_watch = cancel_watch.clone();
            async move {
                if let Some(server) = cancel_watch {
                    until(&server, "no model call's cancel watch attached", |views| {
                        views.iter().any(|view| {
                            let mut target = view.target.split('/');
                            target.next().and_then(lash_service) == Some("LashDurableWaitWorkflow")
                                && target.next_back() == Some("await_resolution")
                        })
                    })
                    .await;
                }
                Ok::<_, LlmTransportError>(reply)
            }
        })
        .build()
        .into_handle();
    let mut builder = lash::LashCore::standard_builder(backend);
    if let Some(tracing) = tracing {
        builder = builder.trace_runtime(tracing);
    }
    builder
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(provider, llm_profile_spec())
        .tools(Arc::new(CountingTool {
            release: Arc::clone(release),
            awaited_child,
            tools: Some(tools.clone()),
        }) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(EnginePluginFactory))
        .plugin(Arc::new(CorpusToolsPlugin::new(tools)))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "replay-corpus",
        ))
        .expect("build the lash core")
}

async fn start_request(core: &lash::LashCore) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: ENGINE_KIND.to_string(),
            payload: json!({}),
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        core.host_artifacts()
            .publish_process_env(
                &lash_core::HostArtifactPin::mint(),
                &lash_core::ProcessExecutionEnvSpec::new(
                    lash_core::AdmittedPluginConfig::default(),
                    lash_core::SessionPolicy {
                        model: Some(lash_core::testing::test_llm_profile_config(
                            llm_profile_spec().wire_model,
                            llm_profile_spec(),
                        )),
                        ..lash_core::SessionPolicy::new(
                            lash::TurnBudget::Unbounded,
                            lash::MaxToolCalls::new(1024),
                        )
                    },
                ),
            )
            .await
            .expect("publish start environment"),
    )
}

/// Waits until the server's invocations satisfy `ready`.
pub(super) async fn until(
    server: &RestateTestServer,
    what: &str,
    ready: impl Fn(&[InvocationView]) -> bool,
) {
    tokio::time::timeout(BOUND, async {
        while !ready(&server.invocations()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: {:#?}", server.invocations()));
}

/// Whether turn `turn_id`'s run finished: some completed `LashTurn` run
/// invocation that names the turn journaled the run's settled lifecycle
/// step (a segment cut by a handover ends without it).
pub(super) fn turn_run_settled(server: &RestateTestServer, turn_id: &str) -> bool {
    server.invocations().iter().any(|view| {
        if view.status != "completed" {
            return false;
        }
        let mut target = view.target.split('/');
        if target.next().and_then(lash_service) != Some("LashTurn")
            || target.next_back() != Some("run")
        {
            return false;
        }
        let entries = server.journal(&view.id).unwrap_or_default();
        entries.iter().any(|entry| {
            entry
                .name
                .as_deref()
                .is_some_and(|name| name.contains(turn_id))
        }) && entries.iter().any(|entry| {
            entry.ty == MessageType::RunCommand
                && entry
                    .name
                    .as_deref()
                    .is_some_and(|name| name.ends_with("lash:run:lifecycle:Settled"))
        })
    })
}

/// The lash service a registered name serves: its stable name or one of its
/// generation lanes.
pub(super) fn lash_service(registered: &str) -> Option<&'static str> {
    LASH_SERVICES
        .iter()
        .map(|service| service.base_name())
        .find(|base| {
            registered == *base
                || registered
                    .strip_prefix(base)
                    .is_some_and(|lane| lane.starts_with("_g"))
        })
}

/// The corpus's session composition on a fresh double: backend, core,
/// process worker, then a created-and-opened session whose wait index is
/// initialized before any turn could race to bootstrap it. Returns the
/// backend, the live core (its drop ends the deployment's runtimes), the
/// open session and the corpus tool evidence.
pub(super) async fn open_scenario_world(
    run_effect_budget: Option<u64>,
    session_id: &'static str,
) -> (
    RestateTestBackend,
    lash::LashCore,
    lash::LashSession,
    Arc<CorpusTools>,
) {
    // The double serves before its workload's core exists. Its fixed stamp
    // must therefore be the generation this same composition really binds.
    let mut config = ServerConfig {
        build_generation: super::current_generation().await,
        ..ServerConfig::default()
    };
    if let Some(budget) = run_effect_budget {
        config = config.with_run_effect_budget(budget);
    }
    let backend = lash_restate_test::backend(SEED, config)
        .await
        .expect("the double over SQLite memory");
    let release = Arc::new(tokio::sync::Semaphore::new(1));
    let tools = Arc::new(CorpusTools::default());
    let core = build_core_with_tools(
        backend.lash_backend(),
        &release,
        None,
        None,
        tools.clone(),
        Some(backend.server().clone()),
    );
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );

    core.session(lash::SessionId::from(session_id))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            llm_profile_spec().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create the session");
    let session = core
        .session(lash::SessionId::from(session_id))
        .open()
        .await
        .expect("open the session");

    // Every session-bearing scope shares this index. Initialize it through
    // one real handler before the turn's wait registrations race to create
    // it; otherwise either journal owns the bootstrap commands.
    backend
        .ingress()
        .call_object_json::<_, crate::Reply<()>>(
            "LashDurableWaitIndex",
            session_id,
            "reinstate",
            &crate::Call::new(()),
        )
        .await
        .expect("initialize the session's wait index");
    (backend, core, session, tools)
}

/// Runs the workload on a fresh double over SQLite memory and returns the
/// deployment once every handler it started has ended.
async fn run_workload() -> (RestateTestBackend, lash_core::engine::BuildGeneration) {
    // The double serves before its workload's core exists. Its fixed stamp
    // must therefore be the generation this same composition really binds.
    let config = ServerConfig {
        build_generation: super::current_generation().await,
        ..ServerConfig::default()
    };
    let backend = lash_restate_test::backend(SEED, config)
        .await
        .expect("the double over SQLite memory");
    let server = backend.server().clone();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let core = build_core(backend.lash_backend(), &release, Some(server.clone()));
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );

    core.session(lash::SessionId::from(SESSION))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            llm_profile_spec().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create the session");
    let session = core
        .session(lash::SessionId::from(SESSION))
        .open()
        .await
        .expect("open the session");

    // Every session-bearing scope shares this index. Initialize it through
    // one real handler before the turn's wait registration and the process's
    // journal pin race to use it; otherwise either journal owns the bootstrap.
    backend
        .ingress()
        .call_object_json::<_, crate::Reply<()>>(
            "LashDurableWaitIndex",
            SESSION,
            "reinstate",
            &crate::Call::new(()),
        )
        .await
        .expect("initialize the session's wait index");

    // The native tool attempt is recorded in the owning turn's journal.
    // The turn is sent unfollowed and attached only once its run settled:
    // an early attach shares a TurnTerminal wait that reaches the index's
    // `settle` only by racing the turn's end — a late attach peeks the
    // resolved terminal and keeps the journal deterministic.
    let turn = session
        .send(lash::TurnInput::text("count once"))
        .id(TURN)
        .await
        .expect("the turn is accepted");
    release.add_permits(1);
    until(&server, "the turn never settled", |_| {
        turn_run_settled(&server, TURN)
    })
    .await;
    let output = tokio::time::timeout(BOUND, turn.output())
        .await
        .expect("the turn's output resolves")
        .expect("the turn succeeds");
    assert_eq!(output.assistant_message(), Some("answered"));
    server.settle().await;

    // The process: started in a handler, where a deployment's starts run.
    let scope = session.turn_scope(lash::TurnId::from(START_TURN));
    let request = start_request(&core).await;
    let started = Arc::new(Mutex::new(None));
    let start: HandlerAttempt = {
        let core = core.clone();
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let request = request.clone();
            let core = core.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let receipt = core
                    .processes()
                    .start(request, scoped)
                    .await
                    .expect("the start commits");
                *started.lock().expect("started lock") = Some(receipt.process_id);
            })
        })
    };
    tokio::time::timeout(
        BOUND,
        backend.run_in_handler(lash_core::AdmittedScope::new(scope.clone()), start),
    )
    .await
    .expect("the start finishes")
    .expect("the start succeeds");
    let process_id = started
        .lock()
        .expect("started lock")
        .clone()
        .expect("the start recorded its process id");
    tokio::time::timeout(BOUND, core.processes().await_output(&process_id))
        .await
        .expect("the process reaches its terminal")
        .expect("the terminal resolves");
    server.settle().await;

    server.settle().await;
    until(&server, "a handler never ended", |views| {
        views.iter().all(|view| view.status == "completed")
    })
    .await;
    (backend, core.build_generation().clone())
}

/// A journal step with its minted ids elided: every run of twelve or more
/// lowercase hex digits (a digest, a minted id, a generation lane) reads `#`.
fn without_ids(step: &str) -> String {
    let mut out = String::with_capacity(step.len());
    let mut run = String::new();
    for ch in step.chars().map(Some).chain([None]) {
        if let Some(digit) = ch.filter(|ch| ch.is_ascii_digit() || ('a'..='f').contains(ch)) {
            run.push(digit);
            continue;
        }
        if run.len() >= 12 {
            out.push('#');
        } else {
            out.push_str(&run);
        }
        run.clear();
        out.extend(ch);
    }
    out
}

/// A call's target as `service/handler`, a lash service under its stable name.
fn call_target(service: &str, handler: &str) -> String {
    format!("{}/{handler}", lash_service(service).unwrap_or(service))
}

/// Per handler, each distinct ordered command sequence its invocations wrote.
pub(super) type HandlerJournals = BTreeMap<String, BTreeSet<Vec<String>>>;

/// What the workload's deployment served and journaled.
pub(super) struct ServiceJournals {
    /// The complete generation the workload's core bound into its engine.
    pub(super) generation: lash_core::engine::BuildGeneration,
    /// Every lash service the deployment registered, by stable name.
    pub(super) served: BTreeSet<String>,
    /// The journals of each lash service's handlers, by stable name.
    pub(super) journals: BTreeMap<String, HandlerJournals>,
}

/// The Run-record families a `ctx.run` completion carries, sniffed off the
/// settled JSON without naming any tool_run type: a `record.events` array's
/// event tags in order, or a `result`-tagged attempt entry as `x:<tag>`,
/// plus `state` when the step's state commands were non-empty.
fn run_record_families(bytes: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    let mut families = Vec::new();
    if let Some(events) = value
        .get("record")
        .and_then(|record| record.get("events"))
        .and_then(serde_json::Value::as_array)
    {
        families.extend(events.iter().filter_map(|event| {
            event
                .get("event")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }));
    } else if let Some(tag) = value
        .get("result")
        .and_then(|result| result.get("result"))
        .and_then(serde_json::Value::as_str)
    {
        families.push(format!("x:{tag}"));
    }
    if value
        .get("state")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|state| !state.is_empty())
    {
        families.push("state".to_string());
    }
    families
}

/// What the deployment served and journaled: every lash service it
/// registered, and each service's per-handler command journals.
pub(super) fn collect(
    server: &RestateTestServer,
) -> (BTreeSet<String>, BTreeMap<String, HandlerJournals>) {
    // The double's own handler host is registered beside lash's services.
    let served = server
        .service_names()
        .iter()
        .filter_map(|name| lash_service(name))
        .map(str::to_owned)
        .collect();
    let mut journals = BTreeMap::<String, HandlerJournals>::new();
    for view in server.invocations() {
        let mut target = view.target.split('/');
        let Some(service) = target.next().and_then(lash_service) else {
            continue;
        };
        let handler = target.next_back().unwrap_or_default();
        let entries = server.journal(&view.id).expect("the invocation's journal");
        // A RunCommand's completion notification can land anywhere in the
        // journal; pair them on the completion id, not on position.
        let completions = entries
            .iter()
            .filter(|entry| entry.ty == MessageType::RunCompletionNotification)
            .filter_map(|entry| {
                entry
                    .completion_id()
                    .and_then(|id| entry.run_completion().map(|result| (id, result)))
            })
            .collect::<BTreeMap<_, _>>();
        let steps = entries
            .into_iter()
            .filter(|entry| entry.ty.is_command())
            .map(|entry| {
                let detail = match entry.ty {
                    MessageType::CallCommand => entry
                        .call_command()
                        .map(|call| call_target(&call.service_name, &call.handler_name)),
                    MessageType::OneWayCallCommand => entry
                        .one_way_call_command()
                        .map(|call| call_target(&call.service_name, &call.handler_name)),
                    MessageType::RunCommand => {
                        let name = entry.name.clone().unwrap_or_default();
                        Some(
                            match entry.completion_id().and_then(|id| completions.get(&id)) {
                                Some(Ok(bytes)) => {
                                    let families = run_record_families(bytes);
                                    if families.is_empty() {
                                        name
                                    } else {
                                        format!("{name} [{}]", families.join(", "))
                                    }
                                }
                                Some(Err(_)) => format!("{name} [failed]"),
                                None => name,
                            },
                        )
                    }
                    _ => entry.written_state_key().or(entry.name),
                };
                match detail {
                    Some(detail) => without_ids(&format!("{:?} {detail}", entry.ty)),
                    None => format!("{:?}", entry.ty),
                }
            })
            .collect();
        journals
            .entry(service.to_owned())
            .or_default()
            .entry(handler.to_owned())
            .or_default()
            .insert(steps);
    }
    (served, journals)
}

/// Runs the workload through the real handlers and reads back the commands
/// each lash service journaled.
pub(super) async fn record() -> ServiceJournals {
    let (backend, generation) = Box::pin(run_workload()).await;
    let (served, journals) = collect(backend.server());
    ServiceJournals {
        generation,
        served,
        journals,
    }
}
