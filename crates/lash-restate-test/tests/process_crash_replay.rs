//! The process crash matrix on Restate, keyed to journal points (FIG-3809).
//!
//! A real Lashlang process runs on the server double: it sleeps, calls a
//! counted tool, waits for the signal `go`, calls the tool again and
//! finishes with the signal's payload. The endpoint cuts every segment after
//! two completed effects, so the process crosses segment boundaries and
//! hands over between `LashProcessWorkflow` invocations.
//!
//! A clean run fixes the reference: its terminal and every segment's
//! journal. Then:
//!
//! * (a) for every journal point of every segment's `run` invocation, a fresh
//!   backend under the same seed drops the attempt just before the server
//!   stores that frame and replays the invocation, with and without
//!   always-replay (every step replays from the journal);
//! * (b) the same points, with a fresh process worker (a new core's, with no
//!   live openers or caches) installed as the crash drops the attempt, as a
//!   restarted deployment comes back;
//! * (c) serial scheduling over 16 seeds under always-replay, with the
//!   process cancelled at a seeded point of its life.
//!
//! Every redrive must reach the reference terminal (or, when cancelled,
//! Cancelled), with no journal mismatch; each tool call runs at most once per
//! completed step, and twice only when the crash lost the step's result.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::ProcessId;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt as _};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, Scheduling, ServerConfig};
use lashlang::testing::ast_builders as b;
use serde_json::json;

const PROCESS_WORKFLOW: &str = "LashProcessWorkflow";
const TOOL: &str = "count_call";
const PROCESS: &str = "main";
const SIGNAL: &str = "go";
/// Completed effects per segment: the sleep and the first tool call fill
/// the first segment, so the signal wait and the second call run in later
/// ones.
const SEGMENT_EFFECT_BUDGET: u64 = 2;

struct CountingTool {
    executions: Arc<AtomicUsize>,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
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
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
    }
}

fn model_spec() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// A core over `restate` whose tool counts into `executions`. The process
/// runs no model; the provider only has to exist.
fn build_core(restate: &RestateTestBackend, executions: &Arc<AtomicUsize>) -> lash::LashCore {
    let backend = restate.lash_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("process-crash-replay")
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
        }) as Arc<dyn lash_core::ToolProvider>)
        .without_queued_work()
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "process-crash-replay",
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

/// ```text
/// process main() signals { go: any } {
///   sleep for "100ms"
///   first = tools.count_call({})
///   value = wait_signal("go")
///   second = tools.count_call({})
///   finish value
/// }
/// ```
async fn publish_process(restate: &RestateTestBackend) -> lash_core::ProcessStartRequest {
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            vec![b::signal(SIGNAL, lashlang::TypeExpr::Any)],
            b::block(vec![
                b::sleep_for(b::string("100ms")),
                b::assign(
                    "first",
                    b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]),
                ),
                b::assign("value", b::wait_signal(SIGNAL)),
                b::assign(
                    "second",
                    b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]),
                ),
                b::finish(b::var("value")),
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
        lashlang::LashlangHostEnvironment::new(
            catalog,
            lashlang::LashlangAbilities::default().with_sleep(),
        ),
    )
    .expect("link the process");
    lashlang::LashlangArtifacts::new(restate.lash_backend().module_artifacts())
        .publish_module_artifact(
            &lash_core::ArtifactOwner::host("process-crash-replay"),
            &linked.artifact,
        )
        .await
        .expect("publish the process artifact");
    let signal_event_types = linked
        .artifact
        .ir()
        .process(PROCESS)
        .map(lash_lashlang_runtime::lashlang_process_signal_event_types)
        .unwrap_or_default();
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
        lash_core::RecoveryContract::Rerunnable,
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
    .with_extra_event_types(
        lash_lashlang_runtime::lashlang_process_event_types()
            .into_iter()
            .chain(signal_event_types),
    )
}

/// Where in the process's life a cancel lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelAt {
    /// Right after the start returns: during the sleep or the first call.
    AfterStart,
    /// While the process waits for the signal.
    OnTheSignalWait,
    /// Right after the signal lands: during the second call or the finish.
    AfterSignal,
}

/// One run of the scenario.
#[derive(Clone, Debug)]
struct Scenario {
    seed: u64,
    config: ServerConfig,
    crash: Option<CrashRule>,
    /// Install a fresh core's process worker as the crash drops the attempt.
    fresh_worker_on_crash: bool,
    cancel: Option<CancelAt>,
}

impl Scenario {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            config: ServerConfig::default(),
            crash: None,
            fresh_worker_on_crash: false,
            cancel: None,
        }
    }
}

/// `(target, entries in journal order)` of one invocation.
type InvocationJournal = (String, Vec<(MessageType, Option<String>)>);

/// What one run observed.
#[derive(Debug)]
struct Run {
    terminal: String,
    tool_executions: usize,
    crashes: u64,
    /// Every `LashProcessWorkflow` invocation's journal, by target.
    journals: Vec<InvocationJournal>,
    /// Invocations that ended failed, with their last failure.
    failures: Vec<String>,
}

/// The terminal a caller sees, without the parts that differ run to run.
fn terminal_label(output: &lash_core::ProcessAwaitOutput) -> String {
    match output {
        lash_core::ProcessAwaitOutput::Settled { output } => match &output.outcome {
            lash_core::ToolCallOutcome::Success(value) => {
                format!("ok {}", value.to_json_value())
            }
            lash_core::ToolCallOutcome::Failure(failure) => format!("failed {}", failure.code),
            lash_core::ToolCallOutcome::Cancelled(_) => "cancelled".to_owned(),
        },
        lash_core::ProcessAwaitOutput::Abandoned { .. } => "abandoned".to_owned(),
        lash_core::ProcessAwaitOutput::NoLongerRetained { terminal_label, .. } => {
            format!("pruned {terminal_label}")
        }
    }
}

/// Run `job` in a handler under a runtime-operation scope named `name`.
async fn in_handler<F>(restate: &RestateTestBackend, name: &str, job: F) -> Result<(), String>
where
    F: for<'a> Fn(
            lash_core::ScopedEffectController<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    let admitted = lash_core::AdmittedScope::runtime_operation(name.to_owned());
    tokio::time::timeout(
        Duration::from_secs(20),
        restate.run_in_handler(admitted, Arc::new(job)),
    )
    .await
    .map_err(|_| format!("`{name}` did not finish"))?
}

async fn wait_for(
    core: &lash::LashCore,
    process_id: &ProcessId,
    matches: impl Fn(&lash_core::facade_support::ObservedProcess) -> bool,
) -> bool {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Some(process)) = core.processes().get(process_id).await
                && matches(&process)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok()
}

async fn run_process(scenario: Scenario) -> Run {
    let restate = lash_restate_test::backend_with_segment_budget(
        scenario.seed,
        scenario.config.clone(),
        SEGMENT_EFFECT_BUDGET,
    )
    .await
    .expect("build the Restate test backend");
    let executions = Arc::new(AtomicUsize::new(0));
    let core = build_core(&restate, &executions);
    restate.install_process_worker(process_worker(&core));
    // Held for the run: the fresh worker belongs to this core.
    let fresh_core = scenario
        .fresh_worker_on_crash
        .then(|| build_core(&restate, &executions));
    if let Some(fresh_core) = &fresh_core {
        let fresh = Mutex::new(Some(process_worker(fresh_core)));
        let slot = restate.process_worker_slot();
        restate.server().on_crash(Arc::new(move |target: &str| {
            if target.starts_with(PROCESS_WORKFLOW)
                && let Some(worker) = fresh.lock().unwrap().take()
            {
                slot.install(worker);
            }
        }));
    }
    if let Some(rule) = scenario.crash.clone() {
        restate.server().crash_on(rule);
    }
    let request = publish_process(&restate).await;
    let started = Arc::new(std::sync::Mutex::new(None::<ProcessId>));
    let mut problems = Vec::new();
    {
        let core = core.clone();
        let request = request.clone();
        let started = Arc::clone(&started);
        if let Err(error) = in_handler(&restate, "start", move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let record = core
                    .processes()
                    .start(request, scoped)
                    .await
                    .expect("start the process");
                *started.lock().unwrap() = Some(record.id);
            })
        })
        .await
        {
            problems.push(format!("start: {error}"));
        }
    }
    // The double mints sequentially, so a start whose answer was lost still
    // names the backend's first process.
    let process_id = started
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| lash_core::ProcessIdMint::sequential_id_for_testing(1));
    let cancel = |name: &'static str| {
        let core = core.clone();
        let process_id = process_id.clone();
        let restate = &restate;
        async move {
            in_handler(restate, name, move |scoped| {
                let core = core.clone();
                let process_id = process_id.clone();
                Box::pin(async move {
                    let _ = core.processes().cancel(&process_id, scoped).await;
                })
            })
            .await
        }
    };
    if scenario.cancel == Some(CancelAt::AfterStart)
        && let Err(error) = cancel("cancel-after-start").await
    {
        problems.push(format!("cancel: {error}"));
    }
    let waiting = wait_for(&core, &process_id, |process| {
        process.terminal()
            || matches!(
                process.wait.as_ref().map(|wait| &wait.kind),
                Some(lash_core::WaitKind::Signal { name, .. }) if name == SIGNAL
            )
    })
    .await;
    if !waiting {
        problems.push("the process never waited for the signal".to_owned());
    }
    if scenario.cancel == Some(CancelAt::OnTheSignalWait)
        && let Err(error) = cancel("cancel-on-wait").await
    {
        problems.push(format!("cancel: {error}"));
    }
    {
        let core = core.clone();
        let process_id = process_id.clone();
        if let Err(error) = in_handler(&restate, "signal", move |scoped| {
            let core = core.clone();
            let process_id = process_id.clone();
            Box::pin(async move {
                let event_type = lash_core::facade_support::process_signal_event_type(SIGNAL)
                    .expect("the signal event type");
                let request =
                    lash_core::ProcessEventAppendRequest::new(event_type, json!({"go": 1}))
                        .with_replay_key(lash_core::facade_support::process_signal_wait_key(
                            &process_id,
                            SIGNAL,
                            "signal-1",
                        ));
                // A signal to a process already cancelled is refused; the
                // terminal says what happened.
                let _ = core
                    .processes()
                    .signal(&process_id, SIGNAL, "signal-1", request, scoped)
                    .await;
            })
        })
        .await
        {
            problems.push(format!("signal: {error}"));
        }
    }
    if scenario.cancel == Some(CancelAt::AfterSignal)
        && let Err(error) = cancel("cancel-after-signal").await
    {
        problems.push(format!("cancel: {error}"));
    }
    let terminal = match tokio::time::timeout(
        Duration::from_secs(20),
        core.processes().await_output(&process_id),
    )
    .await
    {
        Ok(Ok(output)) => terminal_label(&output),
        Ok(Err(error)) => format!("await error: {error}"),
        Err(_) => "stuck".to_owned(),
    };
    let server = restate.server();
    server.settle().await;
    let mut views = server.invocations();
    views.sort_by(|left, right| left.target.cmp(&right.target));
    let mut failures = problems;
    let mut journals = Vec::new();
    for view in views {
        if view.status != "completed" {
            failures.push(format!(
                "{} {} attempts={} last_failure={:?}",
                view.target, view.status, view.attempts, view.last_failure
            ));
        }
        if !view.target.starts_with(PROCESS_WORKFLOW) {
            continue;
        }
        let entries = server
            .journal(&view.id)
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.ty, entry.name))
            .collect();
        journals.push((view.target, entries));
    }
    Run {
        terminal,
        tool_executions: executions.load(Ordering::SeqCst),
        crashes: server.stats().crashes,
        journals,
        failures,
    }
}

/// The workflow key a segment's invocation addresses, from its target
/// (`LashProcessWorkflow/<key>/run`).
fn segment_key(target: &str) -> Option<&str> {
    target
        .strip_prefix(PROCESS_WORKFLOW)?
        .strip_prefix('/')?
        .strip_suffix("/run")
}

/// Every crash point the reference's segments offer: each command a
/// segment's `run` stored and each `ctx.run` result, named, with the run whose
/// result the crash may lose.
fn crash_points(reference: &Run, always_replay: bool) -> Vec<(CrashRule, Option<String>)> {
    let mut points = Vec::new();
    for (target, entries) in &reference.journals {
        let Some(key) = segment_key(target) else {
            continue;
        };
        let rule = |point| segment_crash(key, point, always_replay);
        let commands = entries.iter().filter(|(ty, _)| ty.is_command());
        for (index, (ty, name)) in commands.enumerate().skip(1) {
            // The SDK starts a run's closure as it writes the RunCommand, so
            // a crash before the server stores that command may already have
            // run the effect: it too is a lost run.
            let lost_on_command = (*ty == MessageType::RunCommand)
                .then(|| name.clone())
                .flatten();
            points.push((rule(CrashPoint::BeforeCommand { index }), lost_on_command));
            if *ty == MessageType::RunCommand {
                points.push((
                    rule(CrashPoint::BeforeRunResult { name: name.clone() }),
                    name.clone(),
                ));
            }
        }
    }
    points
}

/// A crash at `point` in the segment keyed `key`, once. Under always-replay
/// every suspension starts a new attempt, so the crash strikes the first
/// attempt that reaches the point rather than the first attempt only.
fn segment_crash(key: &str, point: CrashPoint, always_replay: bool) -> CrashRule {
    let rule = CrashRule::new(point)
        .service(PROCESS_WORKFLOW)
        .handler("run")
        .key(key);
    if always_replay {
        rule
    } else {
        rule.within_attempts(1)
    }
}

/// A lashlang tool-call attempt's run: the one step whose loss re-runs the
/// tool.
fn is_tool_attempt(run: &str) -> bool {
    run.starts_with("lash:lashlang:") && run.contains(":attempt:")
}

async fn reference(seed: u64, config: &ServerConfig) -> Run {
    let reference = run_process(Scenario {
        config: config.clone(),
        ..Scenario::new(seed)
    })
    .await;
    assert_eq!(reference.terminal, r#"ok {"go":1}"#, "{reference:#?}");
    assert_eq!(reference.tool_executions, 2);
    assert_eq!(reference.crashes, 0);
    assert!(reference.failures.is_empty(), "{:#?}", reference.failures);
    let segments = reference
        .journals
        .iter()
        .filter(|(target, _)| segment_key(target).is_some())
        .count();
    assert!(
        segments >= 3,
        "the effect budget cuts the process into segments: {segments}"
    );
    reference
}

/// (a) and (b): every journal point of every segment, crashed once and
/// redriven, reaches the reference terminal with each tool call run once, or
/// twice where the crash lost its result.
async fn every_journal_point_recovers(config: ServerConfig, fresh_worker_on_crash: bool) {
    let seed = 0x3809;
    let started = Instant::now();
    let reference = reference(seed, &config).await;
    let points = crash_points(&reference, config.always_replay);
    assert!(points.len() > 30, "{} crash points", points.len());
    let mut violations = Vec::new();
    for (rule, lost_run) in &points {
        let run = run_process(Scenario {
            config: config.clone(),
            crash: Some(rule.clone()),
            fresh_worker_on_crash,
            ..Scenario::new(seed)
        })
        .await;
        let lost_tool = lost_run.as_deref().is_some_and(is_tool_attempt);
        let expected_tool = if lost_tool { 2..=3 } else { 2..=2 };
        if run.crashes != 1
            || run.terminal != reference.terminal
            || !expected_tool.contains(&run.tool_executions)
            || !run.failures.is_empty()
        {
            violations.push(format!(
                "{:?} {:?} ({lost_run:?}): crashes={} terminal={:?} tool={} failures={:?}",
                rule.key, rule.point, run.crashes, run.terminal, run.tool_executions, run.failures
            ));
        }
    }
    println!(
        "process crash matrix (always_replay={}, fresh_worker={fresh_worker_on_crash}): \
         {} journal points in {:?}, {} violations",
        config.always_replay,
        points.len(),
        started.elapsed(),
        violations.len()
    );
    assert!(
        violations.is_empty(),
        "crash points that did not recover:\n{violations:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_journal_point_of_a_segmented_process_recovers_to_the_reference_terminal() {
    every_journal_point_recovers(ServerConfig::default(), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_journal_point_recovers_when_every_step_replays() {
    every_journal_point_recovers(ServerConfig::default().always_replay(true), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_journal_point_recovers_on_a_fresh_process_worker() {
    every_journal_point_recovers(ServerConfig::default(), true).await;
}

/// (c): one attempt at a time in a seeded order, every step replayed, and
/// the process cancelled at a seeded point. Each seed ends at the reference
/// terminal or cancelled, never with a journal mismatch or a failed
/// invocation; a cancel that lands before the signal always wins.
#[tokio::test(flavor = "current_thread")]
async fn serial_seeds_with_a_cancel_end_at_the_reference_or_cancelled() {
    let config = ServerConfig::default()
        .always_replay(true)
        .scheduling(Scheduling::Serial);
    let reference = reference(0x3809, &config).await;
    let mut violations = Vec::new();
    let mut outcomes = Vec::new();
    for seed in 0..16_u64 {
        let cancel = [
            CancelAt::AfterStart,
            CancelAt::OnTheSignalWait,
            CancelAt::AfterSignal,
        ][usize::try_from(seed % 3).unwrap()];
        let run = run_process(Scenario {
            config: config.clone(),
            cancel: Some(cancel),
            ..Scenario::new(seed)
        })
        .await;
        let cancelled = run.terminal == "cancelled";
        let must_cancel = cancel != CancelAt::AfterSignal;
        outcomes.push(format!("seed {seed} {cancel:?}: {}", run.terminal));
        if !(run.terminal == reference.terminal || cancelled)
            || (must_cancel && !cancelled)
            || run.tool_executions > 2
            || run.crashes != 0
            || !run.failures.is_empty()
        {
            violations.push(format!(
                "seed {seed} {cancel:?}: terminal={:?} tool={} failures={:?}",
                run.terminal, run.tool_executions, run.failures
            ));
        }
    }
    println!("{outcomes:#?}");
    assert!(violations.is_empty(), "{violations:#?}");
}

/// The workflow key of the matrix process's segment `ordinal`: the double
/// mints sequentially, so the process is the backend's first.
fn matrix_segment_key(ordinal: u64) -> String {
    let process_id = lash_core::ProcessIdMint::sequential_id_for_testing(1);
    if ordinal == 0 {
        process_id.to_string()
    } else {
        format!("{process_id}#{ordinal}")
    }
}

/// One crash at `point` in the segment keyed `key`, redriven: the reference
/// terminal, each tool call once, no failed or paused invocation.
async fn one_point_recovers(config: ServerConfig, key: &str, point: CrashPoint) {
    let seed = 0x3809;
    let reference = reference(seed, &config).await;
    let always_replay = config.always_replay;
    let run = run_process(Scenario {
        config,
        crash: Some(segment_crash(key, point, always_replay)),
        ..Scenario::new(seed)
    })
    .await;
    assert_eq!(run.crashes, 1, "{run:#?}");
    assert_eq!(run.terminal, reference.terminal, "{run:#?}");
    assert_eq!(run.tool_executions, 2, "{run:#?}");
    assert!(run.failures.is_empty(), "{:#?}", run.failures);
}

/// B1: the handover step's result is lost after its write landed. The rerun
/// re-derives the handover with a different measured elapsed time in the
/// engine state; the store keeps the first write as the same writer's, and
/// the process carries on (FIG-3809).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_handover_result_rewrites_as_the_same_writer() {
    one_point_recovers(
        ServerConfig::default(),
        &matrix_segment_key(0),
        CrashPoint::BeforeRunResult {
            name: Some("lash.segment.handover".to_owned()),
        },
    )
    .await;
}

/// B2: a crash after a segment retired its handovers, before its output,
/// with every step replayed: the redrive replays its runner from the
/// handover its resume step journaled, with no journal mismatch (FIG-3809).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_between_retire_and_output_redrives_cleanly() {
    let config = ServerConfig::default().always_replay(true);
    let reference = reference(0x3809, &config).await;
    let segment_one = matrix_segment_key(1);
    let (_, entries) = reference
        .journals
        .iter()
        .find(|(target, _)| segment_key(target) == Some(segment_one.as_str()))
        .expect("the reference crosses into segment 1");
    let output = entries
        .iter()
        .filter(|(ty, _)| ty.is_command())
        .position(|(ty, _)| *ty == MessageType::OutputCommand)
        .expect("segment 1 ends with its output");
    one_point_recovers(
        config,
        &segment_one,
        CrashPoint::BeforeCommand { index: output },
    )
    .await;
}
