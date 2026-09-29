//! Two crash windows a clean run never opens (FIG-4095), on the server double
//! and on a live `restate-server` (the `crash-windows` Restate suite).
//!
//! * **The process handover (ADR 0025 §5).** A segment that crosses its
//!   boundary writes its handover in the journaled `lash.segment.handover`
//!   step, then sends its successor. The deployment dies inside that
//!   transition: before the step reaches the journal, after its write landed
//!   but before its result did, or before the successor's send did. The
//!   redrive carries the process to its terminal through exactly one
//!   successor invocation per segment, and no tool call runs twice.
//! * **The presentation put (ADR 0100).** A tool child's presentation step
//!   retains the full output as a content-addressed session artifact inside
//!   the journaled `PresentToolResult` effect, so the `put` lands before the
//!   effect's outcome does. The deployment dies between the two: the redrive
//!   runs the chain again and its `put` converges on the same blob. A second
//!   crash after the outcome is journaled replays the child, which is served
//!   the recorded presentation: the step never runs a third time, the store
//!   holds one blob, the journal one presentation, and the model is shown the
//!   presentation that was recorded.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
// The live leg reads the suite runner's env (RESTATE_INGRESS_URL, endpoint
// binds); ambient env access is sanctioned in test targets.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::{AttachmentStore as _, ProcessId};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt as _};
use lash_restate::RestateNamespace;
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, HandlerAttempt, RestateTestBackend, ServerConfig};
use lashlang::testing::ast_builders as b;
use serde_json::json;

const PROCESS_WORKFLOW: &str = "LashProcessWorkflow";
const HANDOVER_STEP: &str = "lash.segment.handover";
const TOOL: &str = "count_call";
const PROCESS: &str = "main";
/// One completed effect per segment: each tool call ends its segment, so the
/// process hands over twice before it finishes.
const SEGMENT_EFFECT_BUDGET: u64 = 1;
/// A run's wall-time bound on either engine.
const BOUND: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// A lash deployment on the server under test, and the crashes it took.
#[derive(Clone)]
enum Engine {
    Double(RestateTestBackend),
    Live {
        backend: LiveRestateBackend,
        crashes: Arc<AtomicU64>,
        /// The task that brings the deployment back after each crash.
        restarts: tokio::task::AbortHandle,
    },
}

/// One invocation as either server reports it.
#[derive(Debug)]
struct Invocation {
    id: String,
    target: String,
    status: String,
}

impl Engine {
    async fn double(seed: u64, segment_effect_budget: Option<u64>) -> Self {
        let backend = match segment_effect_budget {
            Some(budget) => {
                lash_restate_test::backend_with_segment_budget(
                    seed,
                    ServerConfig::default(),
                    budget,
                )
                .await
            }
            None => lash_restate_test::backend(seed, ServerConfig::default()).await,
        };
        Self::Double(backend.expect("build the Restate test backend"))
    }

    async fn live(label: &str, segment_effect_budget: Option<u64>) -> Self {
        let config = LiveConfig {
            ingress_url: live_env("RESTATE_INGRESS_URL"),
            admin_url: live_env("RESTATE_ADMIN_URL"),
            endpoint_bind: live_env("CW_BIND")
                .parse()
                .expect("a valid endpoint bind address"),
            endpoint_url: live_env("CW_URL"),
            run_tag: run_tag(label),
            namespace: RestateNamespace::default(),
        };
        let backend = match segment_effect_budget {
            Some(budget) => LiveRestateBackend::start_with_segment_budget(config, budget).await,
            None => LiveRestateBackend::start(config).await,
        }
        .expect("start the live backend");
        // A crash kills the deployment where it stands: its endpoint stops
        // serving and the server retries every attempt it ran there. A host
        // comes back after a short outage, and the retries replay into it.
        let crashes = Arc::new(AtomicU64::new(0));
        let (crashed, mut restart) = tokio::sync::mpsc::unbounded_channel::<()>();
        let counted = Arc::clone(&crashes);
        assert!(
            backend.on_crash(Arc::new(move |_target: &str| {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = crashed.send(());
            })),
            "the crash listener is the backend's first"
        );
        let restarts = {
            let backend = backend.clone();
            tokio::spawn(async move {
                while restart.recv().await.is_some() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    backend
                        .start_serving()
                        .await
                        .expect("the crashed deployment serves again");
                }
            })
            .abort_handle()
        };
        Self::Live {
            backend,
            crashes,
            restarts,
        }
    }

    fn lash_backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(backend) => backend.lash_backend(),
            Self::Live { backend, .. } => backend.lash_backend(),
        }
    }

    fn install_process_worker(&self, worker: lash::durability::DurableProcessWorker) {
        match self {
            Self::Double(backend) => backend.install_process_worker(worker),
            Self::Live { backend, .. } => backend.install_process_worker(worker),
        }
    }

    async fn run_in_handler(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(backend) => backend.run_in_handler(admitted, attempt).await,
            Self::Live { backend, .. } => backend.run_in_handler(admitted, attempt).await,
        }
    }

    fn crash_on(&self, rule: CrashRule) {
        match self {
            Self::Double(backend) => backend.server().crash_on(rule),
            Self::Live { backend, .. } => backend.crash_on(rule),
        }
    }

    fn crashes(&self) -> u64 {
        match self {
            Self::Double(backend) => backend.server().stats().crashes,
            Self::Live { crashes, .. } => crashes.load(Ordering::SeqCst),
        }
    }

    fn stores(&self) -> &Arc<lash_sqlite_store::SqliteStoreSet> {
        match self {
            Self::Double(backend) => backend.stores(),
            Self::Live { backend, .. } => backend.stores(),
        }
    }

    /// Wait until the server has nothing left to run.
    async fn settle(&self) {
        match self {
            Self::Double(backend) => backend.server().settle().await,
            Self::Live { backend, .. } => {
                backend
                    .settle(Duration::from_secs(20), Duration::from_millis(200))
                    .await;
            }
        }
    }

    /// Every invocation whose target contains `needle`.
    async fn invocations(&self, needle: &str) -> Vec<Invocation> {
        let all: Vec<Invocation> = match self {
            Self::Double(backend) => backend
                .server()
                .invocations()
                .into_iter()
                .map(|view| Invocation {
                    id: view.id,
                    target: view.target,
                    status: view.status.to_owned(),
                })
                .collect(),
            Self::Live { backend, .. } => backend
                .invocations()
                .await
                .expect("read the server's invocations")
                .into_iter()
                .map(|row| Invocation {
                    id: row.id,
                    target: row.target,
                    status: row.status,
                })
                .collect(),
        };
        all.into_iter()
            .filter(|invocation| invocation.target.contains(needle))
            .collect()
    }

    /// The names of `invocation`'s journaled `ctx.run` commands, in order,
    /// where the server keeps the journal of a completed invocation: the
    /// double does, a live server's default retention does not.
    fn run_names(&self, invocation: &str) -> Option<Vec<String>> {
        match self {
            Self::Double(backend) => Some(
                backend
                    .server()
                    .journal(invocation)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|entry| entry.ty == MessageType::RunCommand)
                    .filter_map(|entry| entry.name)
                    .collect(),
            ),
            Self::Live { .. } => None,
        }
    }

    async fn finish(&self) {
        if let Self::Live {
            backend, restarts, ..
        } = self
        {
            restarts.abort();
            backend.finish().await;
        }
    }
}

fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set by the Restate suite runner"))
}

/// A tag unique to one live run: the server outlives every backend, and a
/// workflow key runs once per server.
fn run_tag(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{label}-{nanos:x}")
}

fn model_spec() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

// ---------------------------------------------------------------------------
// The counted tool
// ---------------------------------------------------------------------------

struct CountingTool {
    executions: Arc<AtomicUsize>,
    output: serde_json::Value,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({}),
    )
    .with_tool_binding(ToolBinding::new(["tools"], TOOL))
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
        self.executions.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(self.output.clone()).into()
    }
}

// ---------------------------------------------------------------------------
// The handover window
// ---------------------------------------------------------------------------

/// Where in a segment's handover the deployment dies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoverCut {
    /// The server never stored the handover step's command; the SDK may
    /// already have started its closure, so its writes may have landed.
    Step,
    /// The handover step's writes landed; its result never reached the
    /// journal.
    StepResult,
    /// The step is journaled; the successor's send is not.
    SuccessorSend,
}

impl HandoverCut {
    const ALL: [Self; 3] = [Self::Step, Self::StepResult, Self::SuccessorSend];

    /// The crash in the `run` of segment `ordinal`, once. The process id is
    /// minted by the start, which sends segment 0, so the crash is armed
    /// before it: segment 0 is the first segment to reach a handover, and a
    /// later segment's key ends `#<ordinal>`. The rule is not bound to the
    /// first attempt: under always-replay a suspension starts a new attempt,
    /// and the crash strikes the first that reaches it.
    fn rule(self, ordinal: u64) -> CrashRule {
        let point = match self {
            Self::Step => CrashPoint::BeforeRun {
                name: HANDOVER_STEP.to_owned(),
            },
            Self::StepResult => CrashPoint::BeforeRunResult {
                name: Some(HANDOVER_STEP.to_owned()),
            },
            // The successor's send is the segment's one one-way call: its
            // tool children run through the effect-group dispatcher's
            // request-response calls.
            Self::SuccessorSend => CrashPoint::BeforeFrame {
                ty: MessageType::OneWayCallCommand,
            },
        };
        let rule = CrashRule::new(point)
            .service(PROCESS_WORKFLOW)
            .handler("run");
        if ordinal == 0 {
            rule
        } else {
            rule.key_ending(format!("#{ordinal}"))
        }
    }
}

/// The workflow key of `process_id`'s segment `ordinal`.
fn segment_key(process_id: &ProcessId, ordinal: u64) -> String {
    if ordinal == 0 {
        process_id.to_string()
    } else {
        format!("{process_id}#{ordinal}")
    }
}

fn process_core(engine: &Engine, executions: &Arc<AtomicUsize>) -> lash::LashCore {
    let backend = engine.lash_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("crash-windows-process")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse::default())
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(model_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(executions),
            output: json!({"result": "counted"}),
        }) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "crash-windows-process",
        ))
        .expect("build the lash core")
}

/// ```text
/// process main() {
///   first = tools.count_call({})
///   second = tools.count_call({})
///   finish second
/// }
/// ```
async fn publish_process(engine: &Engine) -> lash_core::ProcessStartRequest {
    let call = || b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]);
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            Vec::new(),
            b::block(vec![
                b::assign("first", call()),
                b::assign("second", call()),
                b::finish(b::var("second")),
            ]),
        )],
        Vec::new(),
    );
    let contract = tool_definition().contract();
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            TOOL,
            format!("tool:{TOOL}"),
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("link the counted tool");
    let linked = lashlang::LinkedModule::link(
        program,
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default()),
    )
    .expect("link the process");
    lashlang::LashlangArtifacts::new(engine.lash_backend().module_artifacts())
        .publish_module_artifact(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("host pin claim"),
            &linked.artifact,
        )
        .await
        .expect("publish the process artifact");
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref: linked
            .artifact
            .process_ref(PROCESS)
            .expect("the process ref")
            .clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: PROCESS.to_owned(),
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .expect("the process input serializes");
    lash_core::ProcessStartRequest::new(
        input,
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: model_spec(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    ))
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
}

/// A crash at `cut` in the handover of segment `ordinal`: the process
/// reaches its terminal through exactly one invocation of every segment, and
/// each tool call ran once.
async fn a_handover_crash_redrives_one_successor(engine: Engine, ordinal: u64, cut: HandoverCut) {
    let case = format!("segment {ordinal} cut {cut:?}");
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );
    let request = publish_process(&engine).await;

    engine.crash_on(cut.rule(ordinal));
    let started = Arc::new(Mutex::new(None::<ProcessId>));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let receipt = core
                    .processes()
                    .start(request, scoped)
                    .await
                    .expect("start the process");
                *started.lock().unwrap() = Some(receipt.process_id);
            })
        })
    };
    tokio::time::timeout(
        BOUND,
        engine.run_in_handler(
            lash_core::AdmittedScope::runtime_operation(run_tag("start")),
            attempt,
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("{case}: the start did not finish"))
    .unwrap_or_else(|error| panic!("{case}: the start failed: {error}"));
    let process_id = started
        .lock()
        .unwrap()
        .clone()
        .expect("the start recorded its process id");

    let output = tokio::time::timeout(BOUND, core.processes().await_output(&process_id))
        .await
        .unwrap_or_else(|_| panic!("{case}: the process reached no terminal"))
        .unwrap_or_else(|error| panic!("{case}: the terminal did not resolve: {error}"));
    let lash_core::ProcessAwaitOutput::Settled { output } = &output else {
        panic!("{case}: the process ended without output: {output:?}")
    };
    let lash_core::ToolCallOutcome::Success(value) = &output.outcome else {
        panic!("{case}: the process failed: {output:?}")
    };
    assert_eq!(
        value.to_json_value(),
        json!({"result": "counted"}),
        "{case}: the terminal"
    );
    engine.settle().await;

    assert_eq!(engine.crashes(), 1, "{case}: the deployment died once");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "{case}: each tool call ran once; the handover crash lost no tool result"
    );
    let runs = engine
        .invocations(&format!("{PROCESS_WORKFLOW}/{process_id}"))
        .await;
    for invocation in &runs {
        assert_eq!(
            invocation.status, "completed",
            "{case}: every segment ended: {runs:#?}"
        );
    }
    let segment_runs = |ordinal: u64| {
        let target = format!(
            "{PROCESS_WORKFLOW}/{}/run",
            segment_key(&process_id, ordinal)
        );
        runs.iter()
            .filter(|invocation| invocation.target == target)
            .count()
    };
    let segments = (0..)
        .take_while(|ordinal| segment_runs(*ordinal) > 0)
        .count();
    assert_eq!(
        segments, 3,
        "{case}: two tool calls, one per segment, then the finish: {runs:#?}"
    );
    for ordinal in 0..3 {
        assert_eq!(
            segment_runs(ordinal),
            1,
            "{case}: segment {ordinal} ran as exactly one invocation: {runs:#?}"
        );
    }
    let terminals = runs
        .iter()
        .filter(|invocation| {
            invocation.target == format!("{PROCESS_WORKFLOW}/{process_id}/complete_terminal")
        })
        .count();
    assert_eq!(
        terminals, 1,
        "{case}: the terminal is delivered to the stable root once: {runs:#?}"
    );
    engine.finish().await;
}

async fn every_handover_cut_redrives_one_successor(
    engine: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Engine> + Send>>,
) {
    // Segment 0 hands over without a retire step; segment 1 retires its
    // predecessor's handover after its send.
    for ordinal in [0, 1] {
        for cut in HandoverCut::ALL {
            a_handover_crash_redrives_one_successor(engine().await, ordinal, cut).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_in_the_process_handover_redrives_exactly_one_successor() {
    every_handover_cut_redrives_one_successor(|| {
        Box::pin(Engine::double(0x4095, Some(SEGMENT_EFFECT_BUDGET)))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_crash_in_the_process_handover_redrives_exactly_one_successor() {
    every_handover_cut_redrives_one_successor(|| {
        Box::pin(Engine::live("handover", Some(SEGMENT_EFFECT_BUDGET)))
    })
    .await;
}

// ---------------------------------------------------------------------------
// The presentation window
// ---------------------------------------------------------------------------

/// The retained output: long enough to be worth retaining, and the same on
/// every run, so its content address is too.
fn tool_output() -> String {
    "retained tool output ".repeat(64)
}

/// What the presentation step saw: how often it ran and every ref its
/// `retain_text` returned.
#[derive(Default)]
struct StepWitness {
    runs: AtomicUsize,
    retained: Mutex<Vec<lash_core::AttachmentRef>>,
}

/// A presentation step that retains the full output as a session artifact
/// and presents only its ref.
fn retaining_step(witness: Arc<StepWitness>) -> lash_core::plugin::ToolPresentationStep {
    Arc::new(move |input: lash_core::plugin::ToolPresentationInput| {
        let witness = Arc::clone(&witness);
        Box::pin(async move {
            witness.runs.fetch_add(1, Ordering::SeqCst);
            let text = input
                .context
                .output
                .value_for_projection()
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let reference = input
                .context
                .artifacts
                .retain_text("tool-output:retained", &text)
                .await?;
            witness.retained.lock().unwrap().push(reference.clone());
            let mut next = input.previous;
            next.parts = vec![lash_core::facade_support::ModelToolReturnPart::text(
                presented(&reference.id),
            )];
            Ok(next)
        })
    })
}

fn presented(id: &lash_core::AttachmentId) -> String {
    format!("retained as attachment {id}")
}

/// Every model request the turn made.
type Requests = Arc<Mutex<Vec<String>>>;

fn presentation_core(
    engine: &Engine,
    witness: &Arc<StepWitness>,
    executions: &Arc<AtomicUsize>,
    requests: &Requests,
) -> lash::LashCore {
    let provider = {
        let requests = Arc::clone(requests);
        lash_core::testing::TestProvider::builder()
            .kind("crash-windows-presentation")
            .complete(move |request: LlmRequest| {
                let seen = serde_json::to_string(&request.messages).unwrap_or_default();
                let answered = seen.contains("retained as attachment");
                requests.lock().unwrap().push(seen);
                let part = if answered {
                    LlmOutputPart::Text {
                        text: "done".into(),
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
                async move {
                    Ok::<_, LlmTransportError>(LlmResponse {
                        parts: vec![part],
                        response_metadata: Default::default(),
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let spec = lash_core::plugin::PluginSpec::new()
        .with_presentation_step(retaining_step(Arc::clone(witness)));
    lash::LashCore::standard_builder(engine.lash_backend(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(executions),
            output: json!({ "text": tool_output() }),
        }) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "crash-windows-presentation",
            spec,
        )))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "crash-windows-presentation",
        ))
        .expect("build the lash core")
}

/// The deployment dies after the presentation's `put` landed and before its
/// outcome reached the journal, and again after the outcome did.
async fn a_presentation_put_before_its_journal_crash_replays_one_presentation(engine: Engine) {
    let witness = Arc::new(StepWitness::default());
    let executions = Arc::new(AtomicUsize::new(0));
    let requests: Requests = Arc::default();
    let core = presentation_core(&engine, &witness, &executions, &requests);
    // The put lands, the outcome is lost: the child runs its chain again.
    engine.crash_on(CrashRule::new(CrashPoint::BeforeRunResultEnding {
        suffix: ":present".to_owned(),
    }));
    // The outcome is journaled, the child's end is lost: its replay is
    // served the recorded presentation. The child runs on its dispatcher's
    // generation lane, so the rule names the handler, not the service.
    engine.crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        })
        .handler("child"),
    );

    let session_id = run_tag("presentation");
    let session = core
        .session(session_id.as_str())
        .open()
        .await
        .expect("open the session");
    let answer = tokio::time::timeout(
        BOUND,
        session
            .send(lash::TurnInput::text("present once"))
            .id(run_tag("turn").as_str())
            .output(),
    )
    .await
    .expect("the turn finishes")
    .expect("the turn succeeds");
    assert_eq!(
        answer.assistant_message(),
        Some("done"),
        "the turn's answer"
    );
    engine.settle().await;

    assert_eq!(engine.crashes(), 2, "the deployment died at both cuts");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool ran once; its attempt was journaled before either crash"
    );
    // The chain ran on the first attempt (its outcome lost) and on the
    // second (recorded); the replay after the second crash ran nothing.
    assert_eq!(
        witness.runs.load(Ordering::SeqCst),
        2,
        "the step ran again only where its outcome was lost"
    );
    let retained = witness.retained.lock().unwrap().clone();
    assert_eq!(retained.len(), 2, "each run retained the output");
    assert_eq!(
        retained[0], retained[1],
        "the repeated put converged on the same content-addressed ref"
    );
    let reference = &retained[0];
    let blobs = engine
        .stores()
        .attachment_store()
        .list()
        .await
        .expect("list the attachment store");
    assert_eq!(
        blobs.iter().map(|blob| &blob.id).collect::<Vec<_>>(),
        vec![&reference.id],
        "the store holds the one retained blob"
    );

    // The model was shown the recorded presentation.
    let requests = requests.lock().unwrap().clone();
    let shown = presented(&reference.id);
    assert!(
        requests.iter().any(|request| request.contains(&shown)),
        "the model saw `{shown}`: {requests:#?}"
    );

    // One child ran the tool, and its journal records one presentation.
    let children: Vec<_> = engine
        .invocations("EffectGroupDispatch")
        .await
        .into_iter()
        .filter(|invocation| invocation.target.ends_with("/child"))
        .collect();
    assert_eq!(children.len(), 1, "one tool child: {children:#?}");
    assert_eq!(
        children[0].status, "completed",
        "the child ended: {children:#?}"
    );
    if let Some(runs) = engine.run_names(&children[0].id) {
        let presentations: Vec<_> = runs
            .into_iter()
            .filter(|name| name.ends_with(":present"))
            .collect();
        assert_eq!(
            presentations.len(),
            1,
            "the child's journal records one presentation: {presentations:?}"
        );
    }
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_presentation_put_before_its_journal_crash_replays_one_recorded_presentation() {
    a_presentation_put_before_its_journal_crash_replays_one_presentation(
        Engine::double(0x4095, None).await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_presentation_put_before_its_journal_crash_replays_one_recorded_presentation()
 {
    a_presentation_put_before_its_journal_crash_replays_one_presentation(
        Engine::live("presentation", None).await,
    )
    .await;
}
