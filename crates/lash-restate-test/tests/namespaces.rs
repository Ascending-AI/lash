//! Lash deployments share one Restate server in distinct namespaces
//! (FIG-3898).
//!
//! Restate names services per server and hands every new call of a name to
//! the deployment that registered it last, so two lash deployments that
//! serve the same names on one server take over each other's work. Under a
//! namespace, every service a deployment serves and every name it calls is
//! its own (`beta.LashSession`, `beta.LashProcessWorkflow`, ...).
//!
//! The laws, on the server double and on a live `restate-server` (the
//! `namespaces` Restate suite):
//!
//! * Cores in distinct namespaces on one server run a session turn with a
//!   tool call (an effect group), and a process, side by side under the
//!   very same session id, turn id and minted process id, and nothing
//!   crosses: each turn answers from its own model and runs its own tool,
//!   each process runs in its own engine, and every invocation of a
//!   namespace ran on that namespace's deployment.
//! * A registration over names another deployment serves is refused with a
//!   typed error, before anything is registered: the deployment that holds
//!   the names keeps serving them.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
// The live leg reads the suite runner's env (RESTATE_INGRESS_URL, endpoint
// binds); ambient env access is sanctioned in test targets.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate::{RestateNamespace, RestateRegistrationError};
use lash_restate_test::live::{LiveConfig, LiveError, LiveRestateBackend};
use lash_restate_test::{BackendError, HandlerAttempt, RestateTestBackend, ServerConfig};
use serde_json::json;

const TOOL: &str = "count_call";
const ENGINE_KIND: &str = "fig-3898-namespace-recorder";
/// Every core uses the same session id, turn id and (minted in sequence
/// on each core's own stores) process id: only the namespace keeps them
/// apart on the server.
const SESSION: &str = "shared-session";
const TURN: &str = "shared-turn";
const START_TURN: &str = "shared-start";

fn namespace(value: &str) -> RestateNamespace {
    RestateNamespace::new(value).expect("a valid namespace")
}

// ---------------------------------------------------------------------------
// One core's workload
// ---------------------------------------------------------------------------

/// What a core's own model, tool and process engine observed.
#[derive(Default)]
struct Witness {
    llm_calls: AtomicUsize,
    tool_calls: AtomicUsize,
    process_runs: Mutex<Vec<serde_json::Value>>,
}

struct CountingTool {
    witness: Arc<Witness>,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.witness.tool_calls.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
    }
}

/// The process engine of one core: it records every run it executes, with
/// the payload it was started with.
struct RecordingEngine {
    witness: Arc<Witness>,
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for RecordingEngine {
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
        unreachable!("the recording engine stores no artifacts")
    }

    async fn run(
        &self,
        context: lash_core::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        self.witness
            .process_runs
            .lock()
            .unwrap()
            .push(payload.clone());
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(json!({
                    "process_id": context.process_id().as_str(),
                    "started_by": payload["core"],
                })),
            )),
            prelude: Vec::new(),
        })
    }
}

struct EnginePluginFactory {
    witness: Arc<Witness>,
}

impl lash::plugins::PluginFactory for EnginePluginFactory {
    fn id(&self) -> &'static str {
        ENGINE_KIND
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::new(RecordingEngine {
                witness: Arc::clone(&self.witness),
            }) as Arc<dyn lash_core::ProcessEngine>,
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

fn model_spec() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// A stateless scripted model of the core labelled `label`: it calls the
/// tool once, then answers with its own label.
fn model_reply(label: &str, request: &LlmRequest) -> LlmResponse {
    let saw_tool_result = serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains("counted");
    let part = if saw_tool_result {
        LlmOutputPart::Text {
            text: format!("answered by {label}"),
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

fn build_core(backend: lash_core::Backend, label: &str, witness: &Arc<Witness>) -> lash::LashCore {
    let provider = {
        let label = label.to_owned();
        let witness = Arc::clone(witness);
        lash_core::testing::TestProvider::builder()
            .kind("namespaces")
            .complete(move |request: LlmRequest| {
                witness.llm_calls.fetch_add(1, Ordering::SeqCst);
                let reply = model_reply(&label, &request);
                async move { Ok::<_, LlmTransportError>(reply) }
            })
            .build()
            .into_handle()
    };
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model_spec())
        .tools(Arc::new(CountingTool {
            witness: Arc::clone(witness),
        }) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(EnginePluginFactory {
            witness: Arc::clone(witness),
        }))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            format!("namespaces-{label}"),
        ))
        .expect("build the lash core")
}

fn process_worker(core: &lash::LashCore) -> lash::durability::DurableProcessWorker {
    lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("the core's process worker configuration"),
    )
    .expect("build the process worker")
}

fn start_request(label: &str) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: ENGINE_KIND.to_string(),
            payload: json!({ "core": label }),
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: model_spec(),
            ..lash_core::SessionPolicy::new(lash::TurnBudget::Unbounded)
        },
    ))
}

/// A lash deployment on the server under test.
#[derive(Clone)]
enum Deployment {
    Double(RestateTestBackend),
    Live(LiveRestateBackend),
}

impl Deployment {
    fn lash_backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(backend) => backend.lash_backend(),
            Self::Live(backend) => backend.lash_backend(),
        }
    }

    fn install_process_worker(&self, worker: lash::durability::DurableProcessWorker) {
        match self {
            Self::Double(backend) => backend.install_process_worker(worker),
            Self::Live(backend) => backend.install_process_worker(worker),
        }
    }

    async fn run_in_handler(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(backend) => backend.run_in_handler(admitted, attempt).await,
            Self::Live(backend) => backend.run_in_handler(admitted, attempt).await,
        }
    }

    fn namespace(&self) -> RestateNamespace {
        match self {
            Self::Double(backend) => backend.namespace().clone(),
            Self::Live(backend) => backend.namespace().clone(),
        }
    }
}

/// One core on a deployment, with what it observed.
struct Core {
    label: String,
    deployment: Deployment,
    core: lash::LashCore,
    witness: Arc<Witness>,
}

impl Core {
    fn new(label: &str, deployment: Deployment) -> Self {
        let witness = Arc::new(Witness::default());
        let core = build_core(deployment.lash_backend(), label, &witness);
        deployment.install_process_worker(process_worker(&core));
        Self {
            label: label.to_owned(),
            deployment,
            core,
            witness,
        }
    }

    async fn open_session(&self) -> lash::LashSession {
        self.core
            .session(SESSION)
            .open()
            .await
            .unwrap_or_else(|error| panic!("core `{}` opens its session: {error}", self.label))
    }

    /// The session turn: one model call that asks for the tool, the tool as
    /// an effect-group child, and a second call that answers.
    async fn run_turn(&self, session: &lash::LashSession) -> String {
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            session
                .send(lash::TurnInput::text("count once"))
                .id(TURN)
                .output(),
        )
        .await
        .unwrap_or_else(|_| panic!("core `{}`'s turn did not finish", self.label))
        .unwrap_or_else(|error| panic!("core `{}`'s turn failed: {error}", self.label));
        output
            .assistant_message()
            .unwrap_or_else(|| panic!("core `{}`'s turn gave no answer", self.label))
            .to_owned()
    }

    /// Start a process in a handler, where a deployment's starts run, and
    /// await its terminal payload.
    async fn run_process(
        &self,
        session: &lash::LashSession,
    ) -> (lash_core::ProcessId, serde_json::Value) {
        let admitted =
            lash_core::AdmittedScope::new(session.turn_scope(lash::TurnId::from(START_TURN)));
        let request = start_request(&self.label);
        let started = Arc::new(Mutex::new(None));
        let attempt: HandlerAttempt = {
            let core = self.core.clone();
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
                    *started.lock().unwrap() = Some(receipt.process_id);
                })
            })
        };
        tokio::time::timeout(
            Duration::from_secs(60),
            self.deployment.run_in_handler(admitted, attempt),
        )
        .await
        .unwrap_or_else(|_| panic!("core `{}`'s start did not finish", self.label))
        .unwrap_or_else(|error| panic!("core `{}`'s start failed: {error}", self.label));
        let process_id = started
            .lock()
            .unwrap()
            .clone()
            .expect("the start recorded its minted process id");
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            self.core.processes().await_output(&process_id),
        )
        .await
        .unwrap_or_else(|_| panic!("core `{}`'s process reached no terminal", self.label))
        .expect("the terminal resolves");
        let lash_core::ProcessAwaitOutput::Settled { output } = output else {
            panic!(
                "core `{}`'s process ended without output: {output:?}",
                self.label
            )
        };
        let lash_core::ToolCallOutcome::Success(value) = &output.outcome else {
            panic!("core `{}`'s process failed: {output:?}", self.label)
        };
        (process_id, value.to_json_value())
    }
}

/// Run every core's turn and process at once and check that each core saw
/// its own work and nothing else.
async fn run_side_by_side(cores: &[Arc<Core>]) {
    let running: Vec<_> = cores
        .iter()
        .map(|core| {
            let core = Arc::clone(core);
            tokio::spawn(async move {
                let session = core.open_session().await;
                let (answer, (process_id, terminal)) =
                    tokio::join!(core.run_turn(&session), core.run_process(&session));
                (answer, process_id, terminal)
            })
        })
        .collect();
    let mut results = Vec::new();
    for run in running {
        results.push(run.await.expect("a core's workload panicked"));
    }
    let mut process_ids = Vec::new();
    for (core, (answer, process_id, terminal)) in cores.iter().zip(results) {
        let label = &core.label;
        assert_eq!(
            answer,
            format!("answered by {label}"),
            "core `{label}`'s turn is answered by its own model"
        );
        assert_eq!(
            core.witness.llm_calls.load(Ordering::SeqCst),
            2,
            "core `{label}`'s model saw its own turn's two calls and no other core's"
        );
        assert_eq!(
            core.witness.tool_calls.load(Ordering::SeqCst),
            1,
            "core `{label}`'s tool ran once, for its own turn's effect group"
        );
        assert_eq!(
            terminal,
            json!({"process_id": process_id.as_str(), "started_by": label}),
            "core `{label}`'s process terminal is its own"
        );
        // A resumption on a live server's replay leg runs the process body
        // again, so a run count holds only on the double.
        let runs = core.witness.process_runs.lock().unwrap().clone();
        assert!(
            !runs.is_empty() && runs.iter().all(|run| *run == json!({ "core": label })),
            "core `{label}`'s engine ran its own process and no other core's: {runs:?}"
        );
        if matches!(core.deployment, Deployment::Double(_)) {
            assert_eq!(
                runs.len(),
                1,
                "core `{label}`'s engine ran its process once"
            );
        }
        process_ids.push(process_id);
    }
    // On the double's clock each core minted its process id on its own
    // stores from the same sequence: the ids collide, and only the namespace
    // keeps the process workflows apart. A live server's cores mint from the
    // wall clock, so there only the session, turn and effect-group keys,
    // which every core shares by construction, collide.
    if cores
        .iter()
        .all(|core| matches!(core.deployment, Deployment::Double(_)))
    {
        assert!(
            process_ids.windows(2).all(|pair| pair[0] == pair[1]),
            "every core minted the same process id: {process_ids:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// On the server double
// ---------------------------------------------------------------------------

/// Cores in the default namespace and two named ones share one server
/// double: sessions, effect groups and processes under identical ids run
/// side by side, and every invocation of a namespace was pinned to that
/// namespace's deployment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cores_in_distinct_namespaces_share_one_server_without_cross_talk() {
    let first = lash_restate_test::backend(0x3898, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let alpha = first
        .beside(namespace("alpha"), "alpha")
        .await
        .expect("register the alpha deployment beside the first");
    let beta = first
        .beside(namespace("beta"), "beta")
        .await
        .expect("register the beta deployment beside the others");
    let cores = [
        Arc::new(Core::new("default", Deployment::Double(first.clone()))),
        Arc::new(Core::new("alpha", Deployment::Double(alpha))),
        Arc::new(Core::new("beta", Deployment::Double(beta))),
    ];
    run_side_by_side(&cores).await;

    let server = first.server();
    server.settle().await;
    let views = server.invocations();
    for core in &cores {
        let namespace = core.deployment.namespace();
        let own = server
            .route_to(&namespace.service_name("LashSession"))
            .expect("the namespace's session driver is registered");
        let mine: Vec<_> = views
            .iter()
            .filter(|view| {
                let service = view.target.split('/').next().unwrap_or_default();
                if namespace.is_default() {
                    !service.contains('.')
                } else {
                    service.starts_with(&format!("{namespace}."))
                }
            })
            .collect();
        for service in [
            "LashSession",
            "LashTurn",
            "EffectGroupDispatch",
            "EffectGroupIndex",
            "LashProcessWorkflow",
            "LashDurableWaitIndex",
        ] {
            let name = namespace.service_name(service);
            assert!(
                mine.iter().any(|view| {
                    // A generation lane is the name plus `_g<generation>`.
                    let service = view.target.split('/').next().unwrap_or_default();
                    service == name || service.starts_with(&format!("{name}_g"))
                }),
                "core `{}` ran `{name}`: {:?}",
                core.label,
                mine.iter().map(|view| &view.target).collect::<Vec<_>>()
            );
        }
        for view in mine {
            assert_eq!(
                server.pinned_deployment(&view.id).as_ref(),
                Some(&own),
                "core `{}`'s invocation `{}` ran on its own deployment",
                core.label,
                view.target
            );
        }
    }
}

/// A deployment whose namespace another deployment on the server already
/// serves is refused, typed, and registers nothing: the deployment holding
/// the names keeps them. A deployment in a namespace of its own is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registration_over_another_deployments_names_is_refused() {
    let first = lash_restate_test::backend(0x3899, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let server = first.server();
    let held = server
        .route_to("LashSession")
        .expect("the first deployment serves the default namespace");

    let refused = first
        .beside(RestateNamespace::default(), "default-again")
        .await
        .map(|_| ())
        .expect_err("a second deployment of the default namespace is refused");
    assert!(
        matches!(
            &refused,
            BackendError::Registration(error)
                if matches!(
                    &**error,
                    RestateRegistrationError::NameTaken { service, .. }
                        if service == "LashDurableWaitWorkflow"
                )
        ),
        "the refusal names the first contested service: {refused}"
    );
    assert_eq!(
        server.route_to("LashSession"),
        Some(held),
        "the refused deployment took over nothing"
    );
    assert_eq!(server.deployments().len(), 1, "nothing was registered");

    let alpha = first
        .beside(namespace("alpha"), "alpha")
        .await
        .expect("a deployment in a namespace of its own registers");
    let alpha_held = server
        .route_to("alpha.LashSession")
        .expect("the alpha deployment serves its namespace");
    let refused = alpha
        .beside(namespace("alpha"), "alpha-again")
        .await
        .map(|_| ())
        .expect_err("a second deployment of the alpha namespace is refused");
    assert!(
        matches!(
            &refused,
            BackendError::Registration(error)
                if matches!(
                    &**error,
                    RestateRegistrationError::NameTaken { service, .. }
                        if service == "alpha.LashDurableWaitWorkflow"
                )
        ),
        "the refusal names the contested alpha service: {refused}"
    );
    assert_eq!(server.route_to("alpha.LashSession"), Some(alpha_held));
    assert_eq!(server.deployments().len(), 2, "only alpha was added");

    // The deployment that holds the names still runs its work.
    let core = Core::new("alpha", Deployment::Double(alpha));
    let session = core.open_session().await;
    assert_eq!(core.run_turn(&session).await, "answered by alpha");
}

// ---------------------------------------------------------------------------
// On a live restate-server (the `namespaces` Restate suite)
// ---------------------------------------------------------------------------

fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set by the Restate suite runner"))
}

fn run_tag(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{label}-{nanos:x}")
}

/// A live backend in `namespace`, serving on the suite endpoint `endpoint`.
async fn live_backend(
    endpoint: &str,
    namespace: RestateNamespace,
    label: &str,
) -> Result<LiveRestateBackend, LiveError> {
    LiveRestateBackend::start(LiveConfig {
        ingress_url: live_env("RESTATE_INGRESS_URL"),
        admin_url: live_env("RESTATE_ADMIN_URL"),
        endpoint_bind: live_env(&format!("{endpoint}_BIND"))
            .parse()
            .expect("a valid endpoint bind address"),
        endpoint_url: live_env(&format!("{endpoint}_URL")),
        run_tag: run_tag(label),
        namespace,
    })
    .await
}

/// Two cores in distinct namespaces on one live server run sessions,
/// effect groups and processes side by side under identical ids, and each
/// sees only its own work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `namespaces` Restate suite runs it"]
async fn live_restate_cores_in_distinct_namespaces_share_one_server_without_cross_talk() {
    let tag = run_tag("ns");
    let alpha = live_backend("NS_A", namespace(&format!("alpha-{tag}")), "alpha")
        .await
        .expect("register the alpha deployment");
    let beta = live_backend("NS_B", namespace(&format!("beta-{tag}")), "beta")
        .await
        .expect("register the beta deployment beside alpha");
    let cores = [
        Arc::new(Core::new("alpha", Deployment::Live(alpha.clone()))),
        Arc::new(Core::new("beta", Deployment::Live(beta.clone()))),
    ];
    run_side_by_side(&cores).await;
    for backend in [&alpha, &beta] {
        let targets: Vec<String> = backend
            .invocations()
            .await
            .expect("read the namespace's invocations")
            .into_iter()
            .map(|row| row.target)
            .collect();
        for service in [
            "LashSession",
            "LashTurn",
            "EffectGroupDispatch",
            "LashProcessWorkflow",
        ] {
            let name = backend.service_name(service);
            assert!(
                targets.iter().any(|target| {
                    // A generation lane is the name plus `_g<generation>`.
                    let service = target.split('/').next().unwrap_or_default();
                    service == name || service.starts_with(&format!("{name}_g"))
                }),
                "`{name}` ran on the live server: {targets:?}"
            );
        }
    }
    alpha.finish().await;
    beta.finish().await;
}

/// On a live server, a deployment over another deployment's names is
/// refused, typed, and the holder keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `namespaces` Restate suite runs it"]
async fn live_restate_a_registration_over_another_deployments_names_is_refused() {
    let held = namespace(&format!("held-{}", run_tag("ns")));
    let holder = live_backend("NS_A", held.clone(), "holder")
        .await
        .expect("register the holder");
    let refused = live_backend("NS_B", held.clone(), "intruder")
        .await
        .map(|_| ())
        .expect_err("a second deployment of the held namespace is refused");
    assert!(
        matches!(
            &refused,
            LiveError::Registration(error)
                if matches!(
                    &**error,
                    RestateRegistrationError::NameTaken { service, .. }
                        if *service == held.service_name("LashDurableWaitWorkflow")
                )
        ),
        "the refusal names the contested service: {refused}"
    );
    let core = Core::new("holder", Deployment::Live(holder.clone()));
    let session = core.open_session().await;
    assert_eq!(
        core.run_turn(&session).await,
        "answered by holder",
        "the holder still serves its names"
    );
    holder.finish().await;
}
