//! The journals lash's own Restate services write, recorded from the real
//! handlers on the in-process server double (FIG-4805).
//!
//! One workload runs a session turn with a tool call (an effect group), a
//! process and a K4 durable source, each
//! through the deployment's bound services. A service's journal is the
//! ordered commands its handlers wrote; notifications, which the server
//! stores in arrival order, are left out.

use std::collections::{BTreeMap, BTreeSet};
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
const SESSION: &str = "replay-corpus-session";
const TURN: &str = "replay-corpus-turn";
const START_TURN: &str = "replay-corpus-start";
const BOUND: Duration = Duration::from_secs(60);

/// A tool that answers once the workload releases it.
struct CountingTool {
    release: Arc<tokio::sync::Semaphore>,
    awaited_child: bool,
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
        definition
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
        self.release
            .acquire()
            .await
            .expect("the workload keeps the tool's gate open")
            .forget();
        if self.awaited_child {
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
            let start = lash_core::DeclaredStart::new(
                call.context,
                lash_core::StartProcessIntent {
                    owner: call.context.owner().runtime_owner(),
                    declaration,
                },
            )
            .expect("the tool may await its declared child");
            return lash_core::ToolAttemptOutcome::pending(
                lash_core::PendingCompletion::new().resolved_by_declared_start(start),
            );
        }
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
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

fn llm_profile_spec() -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// A stateless scripted model: it calls the tool once, then answers.
fn model_reply(request: &LlmRequest) -> LlmResponse {
    let saw_tool_result = serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains("counted")
        || serde_json::to_string(&request.messages)
            .unwrap_or_default()
            .contains("settled");
    let part = if saw_tool_result {
        LlmOutputPart::Text {
            text: "answered".to_owned(),
            response_meta: None,
        }
    } else {
        LlmOutputPart::ToolCall {
            call_id: "call-1".into(),
            tool_name: TOOL.into(),
            input_json: "{}".into(),
            replay: None,
        }
    };
    LlmResponse {
        parts: vec![part],
        response_metadata: Default::default(),
        ..Default::default()
    }
}

pub(super) fn build_core(
    backend: lash_core::Backend,
    release: &Arc<tokio::sync::Semaphore>,
) -> lash::LashCore {
    build_core_with_trace(backend, release, None, None)
}

pub(in crate::tests) fn build_core_with_trace(
    backend: lash_core::Backend,
    release: &Arc<tokio::sync::Semaphore>,
    tracing: Option<lash_core::facade_support::TraceRuntime>,
    calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
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
            async move { Ok::<_, LlmTransportError>(reply) }
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
        }) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(EnginePluginFactory))
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
async fn until(server: &RestateTestServer, what: &str, ready: impl Fn(&[InvocationView]) -> bool) {
    tokio::time::timeout(BOUND, async {
        while !ready(&server.invocations()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: {:#?}", server.invocations()));
}

/// Whether an invocation waits on the server rather than on its own work.
fn parked(view: &InvocationView) -> bool {
    view.status == "suspended" || view.blocked_on_server == Some(true)
}

fn runs(view: &InvocationView, service: &str, handler: &str) -> bool {
    view.target
        .split('/')
        .next()
        .and_then(lash_service)
        .is_some_and(|served| served == service)
        && view.target.ends_with(&format!("/{handler}"))
}

/// The lash service a registered name serves: its stable name or one of its
/// generation lanes.
fn lash_service(registered: &str) -> Option<&'static str> {
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
    let core = build_core(backend.lash_backend(), &release);
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
    // one real handler before the turn's wait registration and group-child
    // membership race to use it; otherwise either journal owns the bootstrap.
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

    // The turn: a model call that asks for the tool, the tool as an
    // effect-group child, and a second call that answers. The tool answers
    // only once the turn is parked on its group and the child's cancel watch
    // is parked on the index, so every handler takes the parked path.
    let turn = session
        .send(lash::TurnInput::text("count once"))
        .id(TURN)
        .output();
    let tool = async {
        until(&server, "the turn never parked on its group", |views| {
            let ended = |service, handler| {
                views
                    .iter()
                    .any(|view| runs(view, service, handler) && view.status == "completed")
            };
            let parks = |service, handler| {
                views
                    .iter()
                    .any(|view| runs(view, service, handler) && parked(view))
            };
            ended("LashDurableWaitIndex", "register_awakeable")
                && parks("LashTurn", "run")
                && parks("EffectGroupIndex", "await_notice")
                && views.iter().all(|view| {
                    view.status == "completed"
                        || parked(view)
                        || runs(view, "EffectGroupDispatch", "child")
                })
        })
        .await;
        release.add_permits(1);
    };
    let (output, ()) = tokio::time::timeout(BOUND, async { tokio::join!(turn, tool) })
        .await
        .expect("the turn finishes");
    let output = output.expect("the turn succeeds");
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

/// Runs the workload through the real handlers and reads back the commands
/// each lash service journaled.
pub(super) async fn record() -> ServiceJournals {
    let (backend, generation) = Box::pin(run_workload()).await;
    let server = backend.server();
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
        let steps = server
            .journal(&view.id)
            .expect("the invocation's journal")
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
    ServiceJournals {
        generation,
        served,
        journals,
    }
}
