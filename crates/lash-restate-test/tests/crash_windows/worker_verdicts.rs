//! Live worker verdicts are never recorded outcomes (FIG-4451).
//!
//! A worker-budget or pool-capacity verdict is this host's, read live and
//! outside any recorded step: a replay, or another host with capacity,
//! decides differently. So a refused reservation or a budget spent mid-run
//! fails the attempt retryably and records nothing; the attempt that finds
//! capacity records what the code did.
//!
//! * **An RLM cell.** The host's accounting refuses the cell's first
//!   reservation. The replay reserves, runs the cell and finishes the turn
//!   with it: the model never sees the refusal.
//! * **A process body.** The host's cumulative worker CPU budget is spent
//!   while the body runs, and every later reservation on that host is
//!   refused. The process stays unsettled until a host with capacity runs
//!   its segment, which stores the body's terminal after the body's own
//!   commands.

use super::recovery::{
    segment_journals_end_where_their_bodies_ran, start, terminal_fact_is_settled, worker,
};
use super::*;
use lash_core::store::worker_recovery::{
    WorkerRecoveryClaim, WorkerRecoveryError, WorkerRecoveryLimits, WorkerRecoveryStore,
    WorkerRecoveryTotals,
};
use lash_protocol_rlm::Dialect as _;

/// Which live refusal the host's accounting answers the first reservation
/// with.
#[derive(Clone, Copy, Debug)]
enum Refusal {
    Attempts,
    Cpu,
}

impl Refusal {
    fn error(self) -> WorkerRecoveryError {
        match self {
            Self::Attempts => WorkerRecoveryError::AttemptsExhausted,
            Self::Cpu => WorkerRecoveryError::CpuExhausted,
        }
    }
}

/// The host's worker accounting: it refuses its first reservation with
/// `refusal`, as a host whose live attempt or CPU accounting is spent does,
/// and answers every later one from the store.
struct RefusesFirstReservation {
    inner: Arc<dyn WorkerRecoveryStore>,
    refusal: Refusal,
    refused: Mutex<Option<String>>,
    reserved: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl WorkerRecoveryStore for RefusesFirstReservation {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        {
            let mut refused = self.refused.lock().unwrap();
            if refused.is_none() {
                *refused = Some(scope.to_owned());
                return Err(self.refusal.error());
            }
        }
        let claim = self.inner.reserve(scope, limits).await?;
        self.reserved.lock().unwrap().push(scope.to_owned());
        Ok(claim)
    }
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        self.inner.mark_running(claim).await
    }
    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError> {
        self.inner.settle(claim, totals).await
    }
}

/// The store's own accounting, counting the reservations it refused.
struct CountsRefusals {
    inner: Arc<dyn WorkerRecoveryStore>,
    refused: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl WorkerRecoveryStore for CountsRefusals {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        let reserved = self.inner.reserve(scope, limits).await;
        if matches!(
            reserved,
            Err(WorkerRecoveryError::CpuExhausted | WorkerRecoveryError::AttemptsExhausted)
        ) {
            self.refused.fetch_add(1, Ordering::SeqCst);
        }
        reserved
    }
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        self.inner.mark_running(claim).await
    }
    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError> {
        self.inner.settle(claim, totals).await
    }
}

/// Counts the runtime's proposals of a bound exhaustion as an execution's
/// recorded outcome. A test build makes each proposal loud: the runtime's
/// confidence tripwire panics the attempt with `exhausted a required Lashlang
/// bound` instead of recording it, and the engine retries the panic, so the
/// law counts the panics.
fn bound_exhaustion_proposals() -> Arc<AtomicUsize> {
    let proposals = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&proposals);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if message.contains("exhausted a required Lashlang bound") {
            counted.fetch_add(1, Ordering::SeqCst);
        }
        previous(info);
    }));
    proposals
}

// ---------------------------------------------------------------------------
// The RLM cell
// ---------------------------------------------------------------------------

const CELL: &str =
    "<typescript>const first = await tools.count_call({}); finish({ first });</typescript>";

/// The host refuses the cell's first reservation. The turn finishes with the
/// cell's own result, the model is asked once, the tool runs once, and the
/// refused scope is reserved again by the replay that ran the cell.
async fn a_refused_cell_reservation_replays_into_a_host_with_capacity(
    engine: Engine,
    refusal: Refusal,
) {
    let case = format!("cell refusal {refusal:?}");
    let accounting = Arc::new(RefusesFirstReservation {
        inner: engine.lash_backend().worker_recovery(),
        refusal,
        refused: Mutex::new(None),
        reserved: Mutex::new(Vec::new()),
    });
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine.lash_backend())
        .map_worker_recovery({
            let accounting = Arc::clone(&accounting);
            move |_| accounting as Arc<dyn WorkerRecoveryStore>
        })
        .into_backend();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("worker-verdict-cell")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, LlmTransportError>(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: CELL.into(),
                            response_meta: None,
                        }],
                        terminal_reason: lash_core::LlmTerminalReason::Stop,
                        ..Default::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let rlm = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let executions = Arc::new(AtomicUsize::new(0));
    let core = lash::LashCore::rlm_builder(backend, rlm)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(provider, llm_profile_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(&executions),
            output: json!({"result": "counted"}),
        }) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "worker-verdict-cell",
            "test",
        ))
        .expect("build the lash core");
    let session = created_session(
        &core,
        lash_core::SessionId::fixture(run_tag("worker-verdict-cell")),
    )
    .await
    .open()
    .await
    .expect("open the session");
    let output = tokio::time::timeout(
        BOUND,
        session
            .send(lash::TurnInput::text("count once"))
            .id("worker-verdict-root")
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("{case}: the turn did not finish"))
    .unwrap_or_else(|error| panic!("{case}: the turn failed: {error}"));
    assert!(
        matches!(
            output.result.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ),
        "{case}: the turn finished: {:?}",
        output.result.errors
    );
    assert_eq!(
        output.final_value(),
        Some(&json!({"first": {"result": "counted"}})),
        "{case}: the turn finished with the cell's own result"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "{case}: the model was asked once; it never saw the host's refusal"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "{case}: the cell's tool call ran once"
    );
    let refused = accounting
        .refused
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| panic!("{case}: the host refused a reservation"));
    assert!(
        accounting.reserved.lock().unwrap().contains(&refused),
        "{case}: the replay reserved the refused cell scope {refused} and ran the cell"
    );
    Box::pin(session.close()).await.expect("close the session");
    engine.finish().await;
}

async fn every_cell_refusal_replays_into_a_host_with_capacity(
    engine: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Engine> + Send>>,
) {
    for refusal in [Refusal::Attempts, Refusal::Cpu] {
        a_refused_cell_reservation_replays_into_a_host_with_capacity(engine().await, refusal).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_cell_reservation_is_never_the_cells_recorded_result() {
    every_cell_refusal_replays_into_a_host_with_capacity(|| Box::pin(Engine::double(0x4451, None)))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_refused_cell_reservation_is_never_the_cells_recorded_result() {
    every_cell_refusal_replays_into_a_host_with_capacity(|| {
        Box::pin(Engine::live("worker-verdict-cell", None))
    })
    .await;
}

// ---------------------------------------------------------------------------
// The process body
// ---------------------------------------------------------------------------

/// Rounds of native string work the body burns before its tool call: each
/// splits and rejoins a string of `WIDTH` items. The rounds cost far more
/// worker CPU than the spent host's budget, far less than the standard one,
/// and few enough execution observations to stay inside the protocol's
/// payload bound.
const ROUNDS: f64 = 30.0;
const WIDTH: usize = 20_000;

/// The spent host's cumulative worker CPU budget per execution scope.
const SPENT_CPU: Duration = Duration::from_millis(15);

/// A host running process segments with `workers` over `backend`.
pub(super) fn process_host(
    backend: lash_core::Backend,
    workers: lash::rlm::WorkerService,
    executions: &Arc<AtomicUsize>,
) -> lash::LashCore {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("worker-verdict-process")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse::default())
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                1_000_000_000,
            ))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(512))
            .build(),
        Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    )
    .with_worker_service(workers);
    lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(provider, llm_profile_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(executions),
            output: json!({"result": "counted"}),
        }) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "worker-verdict-process",
        ))
        .expect("build the lash core")
}

/// The dialect's worker service with `cumulative_cpu` per execution scope.
pub(super) fn workers_with_cpu(cumulative_cpu: Duration) -> lash::rlm::WorkerService {
    let mut config = lash_protocol_rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.deadlines.cumulative_cpu = cumulative_cpu;
    lash::rlm::WorkerService::new(config)
}

/// ```text
/// process main() {
///   s = "x,x,…"                      // WIDTH items
///   i = 0
///   while (i < ROUNDS) { s = join(split(s, ","), ","); i = i + 1 }
///   first = tools.count_call({})
///   finish first
/// }
/// ```
async fn burning_request(engine: &Engine) -> lash_core::ProcessStartRequest {
    use lashlang::JavaScriptBinaryOp::{Add, Less};
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            Vec::new(),
            b::block(vec![
                b::assign("s", b::string(&"x,".repeat(WIDTH))),
                b::assign("i", b::num(0.0)),
                b::while_loop(
                    b::binary(b::var("i"), Less, b::num(ROUNDS)),
                    b::block(vec![
                        b::assign(
                            "s",
                            b::builtin(
                                "join",
                                vec![
                                    b::builtin("split", vec![b::var("s"), b::string(",")]),
                                    b::string(","),
                                ],
                            ),
                        ),
                        b::assign("i", b::binary(b::var("i"), Add, b::num(1.0))),
                    ]),
                ),
                b::assign(
                    "first",
                    b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]),
                ),
                b::finish(b::var("first")),
            ]),
        )],
        Vec::new(),
    );
    let input = publish_program(engine, program, lashlang::LashlangAbilities::default()).await;
    lash_core::ProcessStartRequest::new(
        input.into_process_input().expect("the process input"),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        lash_core::publish_process_execution_env(
            engine.lash_backend().process_env_store().as_ref(),
            &lash_core::testing::host_pin_claim_for_testing(),
            &process_env_spec(),
        )
        .await
        .expect("publish the captured environment"),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
}

/// The body spends the first host's CPU budget mid-run, and that host
/// refuses every later reservation of the segment. Neither verdict is the
/// process's terminal: the process stays unsettled until a host with
/// capacity runs the segment, which completes it with the body's result
/// after the body's own commands.
async fn a_spent_cpu_budget_is_never_the_process_terminal(engine: Engine) {
    let proposals = bound_exhaustion_proposals();
    let executions = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let spent_backend =
        lash_core::testing::runtime_helpers::LayeredBackend::over(engine.lash_backend())
            .map_worker_recovery({
                let refused = Arc::clone(&refused);
                move |inner| {
                    Arc::new(CountsRefusals { inner, refused }) as Arc<dyn WorkerRecoveryStore>
                }
            })
            .into_backend();
    let spent = process_host(spent_backend, workers_with_cpu(SPENT_CPU), &executions);
    engine.install_process_worker(worker(&spent));
    let request = burning_request(&engine).await;
    let id = start(&engine, &spent, request).await;

    // The spent host meets the budget mid-run, then refuses the segment's
    // next reservation; or it proposes its live verdict as the process
    // terminal.
    let registry = engine.lash_backend().process_registry();
    let met = tokio::time::timeout(BOUND, async {
        loop {
            let record = registry
                .get_process(&id)
                .await
                .expect("read the process")
                .expect("the process exists");
            if record.is_terminal()
                || proposals.load(Ordering::SeqCst) > 0
                || refused.load(Ordering::SeqCst) > 0
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the spent host met its CPU budget");
    assert!(
        !met.is_terminal() && proposals.load(Ordering::SeqCst) == 0,
        "a live worker verdict became the process terminal: {:?}",
        met.outcome()
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the spent host never reached the tool call"
    );

    let capable = process_host(
        engine.lash_backend(),
        workers_with_cpu(lash::rlm::WorkerDeadlines::standard().cumulative_cpu),
        &executions,
    );
    engine.install_process_worker(worker(&capable));
    let output = tokio::time::timeout(BOUND, capable.processes().await_output(&id))
        .await
        .expect("the process reached its terminal")
        .expect("the terminal resolved");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without output: {output:?}")
    };
    let lash_core::ToolCallOutcome::Success(value) = &output.outcome else {
        panic!("the process failed: {output:?}")
    };
    assert_eq!(
        value.to_json_value(),
        json!({"result": "counted"}),
        "the terminal is the body's result"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool call ran once, on the host with capacity"
    );
    assert_eq!(
        proposals.load(Ordering::SeqCst),
        0,
        "no attempt proposed a worker verdict as the process terminal"
    );
    let runs = engine.completed_process_invocations(&id).await;
    segment_journals_end_where_their_bodies_ran(&engine, &runs).await;
    terminal_fact_is_settled(&engine, &id).await;
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spent_worker_cpu_budget_is_never_the_process_terminal() {
    a_spent_cpu_budget_is_never_the_process_terminal(Engine::double(0x4451, None).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_spent_worker_cpu_budget_is_never_the_process_terminal() {
    a_spent_cpu_budget_is_never_the_process_terminal(
        Engine::live("worker-verdict-process", None).await,
    )
    .await;
}
