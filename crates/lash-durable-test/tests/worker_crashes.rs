//! A VM worker's death during a cell's setup fails the attempt, never the
//! cell (FIG-4459), through a host's `send()` with the core's node serving
//! the turn.
//!
//! The pool has one worker, prewarmed by a separate session. Once the model
//! has answered with the tested cell, that worker is killed before dialect
//! lowering or machine start. The request meets the real
//! closed transport: a host verdict, read live outside any recorded step,
//! so the attempt fails retryably and seals nothing. The retried pass runs
//! the cell on a replacement worker: the turn finishes with the cell's own
//! result, the model is asked once and never sees a failure, and the cell's
//! tool runs once.
//!
//! The same served turn, with no worker killed, is what a cell costs the
//! worker boundary: one lease and a counted number of frames (FIG-5921).
//!
//! A worker lost to a limit lash itself set on it, its CPU budget, its
//! compute deadline or the wall time its execution may hold workers, is the
//! cell's own end, never a crash: the cell is refused once with that limit,
//! and the model reads it (FIG-6523, FIG-6786). So is a worker the cell's
//! own run killed, as an allocation past the worker's address-space ceiling
//! does: before, the turn ran the cell again pass after pass until the
//! session parked, and the model never saw it (FIG-6786).
//!
//! A worker killed from outside its cell after the cell began computing,
//! with no limit of lash's in sight, is no end of the cell's own: the turn
//! runs the cell again from its journal on a fresh worker, whose calls
//! replay rather than run again. Only a cell whose worker is lost on every
//! one of its attempts is refused, and its refusal does not blame the cell
//! (FIG-6806). The lost pass publishes no end of the cell, so a follower of
//! the session's live replay reads one consistent history (FIG-6891).
//!
//! A worker names a page it sealed earlier in its activation under the
//! grant the parent gave it, without sealing it again. What a later park
//! of the activation commits stays stored while the activation can still
//! name it: retirement of the root that first published the page leaves it
//! in place until the activation ends (FIG-6629).
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolCall;
use lash_core::ToolOutcome;
use lash_core::llm::types::LlmRequest;
use lash_kernel_value::tool_contract::ExecutionPolicy;
use served::{Tier, World};

const TOOL: &str = "count_call";

const TOOL_OPERATION: &str = "tools.count_call";
const CELL: &str = "const first = await tools.countCall({}); await control.finish({ first });";

/// `count_call`'s body: it counts its runs and answers `counted`.
struct Count {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl StaticToolExecute for Count {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(serde_json::json!({ "result": "counted" })).into()
    }
}

fn count(runs: &Arc<AtomicUsize>) -> Arc<dyn lash_core::ToolProvider> {
    count_call(Count {
        runs: Arc::clone(runs),
    })
}

/// `count_call`, run by `body`.
fn count_call(body: impl StaticToolExecute + 'static) -> Arc<dyn lash_core::ToolProvider> {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        TOOL,
        TOOL_OPERATION,
        "Counts its calls.",
        serde_json::json!({ "type": "object", "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("count_call's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once);
    Arc::new(StaticToolProvider::new(vec![definition], body))
}

/// The pool's one worker, killed once before a chosen checkout of the
/// cell's.
struct Killer {
    workers: lash::vm::WorkerService,
    /// Set once the model has answered: the cell's requests follow.
    armed: AtomicBool,
    /// The checkouts recorded before the cell's first request.
    before_cell: AtomicUsize,
    /// How many of the cell's checkouts the worker answers before it dies.
    before_request: usize,
    /// The pid it killed, and the index of the checkout that met it.
    killed: Mutex<Option<(u32, usize)>>,
}

impl Killer {
    fn arm(&self) {
        if !self.armed.swap(true, Ordering::SeqCst) {
            self.before_cell
                .store(self.workers.worker_receipts().len(), Ordering::SeqCst);
        }
    }

    /// Called as each worker call begins: kills the sole idle worker once
    /// the cell has had `before_request` of its requests checked out.
    #[expect(
        clippy::disallowed_methods,
        reason = "the law kills a real worker process, so its next request meets a closed transport"
    )]
    fn on_call(&self) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let receipts = self.workers.worker_receipts();
        let mut killed = self.killed.lock().unwrap();
        if killed.is_some()
            || receipts.len() - self.before_cell.load(Ordering::SeqCst) != self.before_request
        {
            return;
        }
        let pid = receipts
            .last()
            .expect("session setup already used the sole worker")
            .pid;
        assert!(
            std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .expect("signal the worker")
                .success(),
            "kill the worker before the cell's request"
        );
        *killed = Some((pid, receipts.len()));
    }
}

/// The model: one cell that calls the tool once and finishes with its
/// answer. Each call arms the killer.
fn model(
    killer: &Arc<Killer>,
    calls: &Arc<AtomicUsize>,
) -> lash_core::facade_support::ProviderHandle {
    let killer = Arc::clone(killer);
    let calls = Arc::clone(calls);
    let warming = AtomicBool::new(true);
    lash_core::testing::TestProvider::builder()
        .kind("worker-setup-crash")
        .complete(move |_request: LlmRequest| {
            let response = if warming.swap(false, Ordering::SeqCst) {
                served::cell("await control.finish(\"warm\");")
            } else {
                calls.fetch_add(1, Ordering::SeqCst);
                killer.arm();
                served::cell(CELL)
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle()
}

/// The kernel worker request the worker dies before: dialect lowering,
/// or starting the cell's machine.
#[derive(Clone, Copy, Debug)]
enum Request {
    Lower,
    Cell,
}

impl Request {
    /// A fresh kernel cell lowers its source, then starts its machine.
    /// Compilation is part of that start, not a separate worker request.
    fn before(self) -> usize {
        match self {
            Self::Lower => 0,
            Self::Cell => 1,
        }
    }
}

async fn setup_crash_case(tier: Tier, request: Request) {
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.max_workers = 1;
    let receipts = lash::vm::WorkerService::new(config).with_worker_receipts();
    let killer = Arc::new(Killer {
        workers: receipts.clone(),
        armed: AtomicBool::new(false),
        before_cell: AtomicUsize::new(0),
        before_request: request.before(),
        killed: Mutex::new(None),
    });
    let workers = receipts.with_call_hold({
        let killer = Arc::clone(&killer);
        Arc::new(move || {
            killer.on_call();
            Box::new(())
        })
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let derivations = Arc::new(AtomicUsize::new(0));
    let plugin = Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("worker-receipt"),
        lash::plugins::PluginSpec::new().with_assistant_response(
            lash::hook_key!("count-derivations"),
            None,
            Arc::new({
                let derivations = Arc::clone(&derivations);
                let killer = Arc::clone(&killer);
                move |ctx| {
                    if killer.armed.load(Ordering::SeqCst) {
                        derivations.fetch_add(1, Ordering::SeqCst);
                    }
                    Box::pin(async move {
                        Ok(lash::plugins::AssistantResponseTransform {
                            response: ctx.response,
                            events: Vec::new(),
                        })
                    })
                }
            }),
        ),
    ));
    let Some(world) = World::with_model(tier, Vec::new(), model(&killer, &calls), |backend| {
        lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers.clone()))
            .tools(lash::tool_provider_key!("tools"), count(&runs))
            .plugin(plugin.clone())
    })
    .await
    else {
        return;
    };
    // Populate the idle pool through an ordinary cell, before arming the
    // killer. Session setup itself makes no kernel-worker checkout.
    let warm = world.session("worker-prewarm", served::spec(8)).await;
    let warmed = world.send(&warm, "warm the worker").await;
    assert_eq!(
        warmed.finished().map(|(_, value)| value),
        Some(&serde_json::json!("warm"))
    );
    let session = world.session("worker-setup-crash", served::spec(8)).await;
    let output = world.send(&session, "count once").await;

    let (killed, met) = killer
        .killed
        .lock()
        .unwrap()
        .expect("the worker was killed");
    let receipts = workers.worker_receipts();
    // The receipt's path is the worker client's type, which the facade does
    // not export: its name is enough here.
    assert_eq!(
        receipts
            .get(met)
            .map(|receipt| (format!("{:?}", receipt.path), receipt.pid)),
        Some((format!("{request:?}"), killed)),
        "the killed worker met the cell's {request:?} request: {receipts:?}"
    );
    assert!(
        receipts[met..].iter().any(|receipt| receipt.pid != killed),
        "a replacement worker completes the cell: {receipts:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the model never sees a recorded host failure from the crash"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the cell's tool runs once");
    assert_eq!(
        derivations.load(Ordering::SeqCst),
        2,
        "the worker retry derives the paid receipt again, with no model.done fallback"
    );
    assert!(
        output.outcome.error_cause().is_none(),
        "no cell failures: {:?}",
        output.outcome.error_cause()
    );
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!({ "first": { "result": "counted" } }))
    );
    world.shutdown().await;
}

/// FIG-5921: a cell that calls one tool and finishes crosses the worker
/// boundary on one lease, in at most 25 frames with one reset: its lowering
/// and its run share the lease, the run's start crosses with its first
/// slice, each park's outcome with the next slice, and each park's state
/// with the park. It took two leases, two resets and 49 frames when each
/// crossed alone.
async fn a_tool_and_finish_cell_crosses_the_worker_boundary_on_one_lease(tier: Tier) {
    let workers = lash::vm::WorkerService::default();
    let warming = AtomicBool::new(true);
    let model = lash_core::testing::TestProvider::builder()
        .kind("worker-exchanges")
        .complete(move |_request: LlmRequest| {
            let response = if warming.swap(false, Ordering::SeqCst) {
                served::cell("await control.finish(\"warm\");")
            } else {
                served::cell(CELL)
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle();
    let runs = Arc::new(AtomicUsize::new(0));
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers.clone()))
            .tools(lash::tool_provider_key!("tools"), count(&runs))
    })
    .await
    else {
        return;
    };
    // A first cell starts the pool and warms its worker.
    let warm = world
        .session("worker-exchanges-warm", served::spec(8))
        .await;
    world.send(&warm, "warm the worker").await;
    let counters = || workers.pool().expect("the pool").measurements().counters;
    let before = counters();
    let session = world.session("worker-exchanges", served::spec(8)).await;
    let output = world.send(&session, "count once").await;
    let after = counters();
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!({ "first": { "result": "counted" } }))
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the cell's tool runs once");
    let frames = after.ipc_sent_messages + after.ipc_received_messages
        - before.ipc_sent_messages
        - before.ipc_received_messages;
    assert_eq!(
        after.checkouts - before.checkouts,
        1,
        "one lease: {after:?}"
    );
    assert_eq!(after.resets - before.resets, 1, "one reset: {after:?}");
    eprintln!("worker-boundary cell: {frames} frames");
    assert!(frames <= 25, "{frames} frames: {after:?}");
    world.shutdown().await;
}

/// A cell that computes without growing its live state.
const HEAVY: &str = "let n = 0; while (n >= 0) { n = n + 1; } await control.finish(n);";

/// Each failed match backtracks over fourteen units, below the matcher's
/// work guard, with little charge and no growing allocation. Repeating it
/// spends the worker's CPU, compute or wall bound before the pool's 50M charge
/// ceiling; an arithmetic loop can spend that charge in under a second.
const BACKTRACK_COMPUTE: &str = "let n = 0; while (n >= 0) { /(a+)+b/.test(\"aaaaaaaaaaaaaa\"); n = n + 1; } await control.finish(n);";

/// A cell whose regular expression backtracks over 8 million units: the
/// matcher's stack, which no run bound meters, grows past a 1 GiB
/// address-space ceiling long before the match reaches its guard.
const BACKTRACK: &str =
    "const input = \"a\".repeat(8000000); await control.finish(/^(a|aa)+$b/.test(input));";

/// A limit lash sets on the cell's worker, configured small.
#[derive(Clone, Copy, Debug)]
enum Limit {
    /// The execution's CPU budget: the kernel stops the worker at the CPU
    /// ceiling the worker set from it (`SIGXCPU`).
    Cpu,
    /// One exchange's compute deadline: the parent stops waiting.
    Deadline,
    /// The wall time the execution may hold workers (FIG-6786): the
    /// parent stops waiting within a compute deadline it never reaches.
    Wall,
    /// No limit of lash's run: the worker's address-space ceiling, which
    /// the cell's allocation passes, so the worker aborts (FIG-6786).
    Died,
}

impl Limit {
    fn configure(self, config: &mut lash::vm::WorkerPoolConfig) {
        let second = std::time::Duration::from_secs(1);
        match self {
            Self::Cpu => config.deadlines.cumulative_cpu = second,
            Self::Deadline => config.deadlines.compute = second,
            Self::Wall => config.deadlines.execution_wall = second,
            Self::Died => {
                config.confinement.address_space_bytes = 1024 * 1024 * 1024;
                // Room for the parser stack of the largest admitted source.
                config.protocol.max_source_bytes = 4096;
                config.tuning.typescript_source_bytes = 4096;
            }
        }
    }

    /// The cell the model writes.
    fn cell(self) -> &'static str {
        match self {
            Self::Died => BACKTRACK,
            Self::Cpu | Self::Deadline | Self::Wall => BACKTRACK_COMPUTE,
        }
    }

    /// The typed limit the cell's failure carries, as it encodes.
    fn typed(self) -> serde_json::Value {
        match self {
            Self::Cpu => serde_json::json!({ "cpu": { "bound_ms": 1000 } }),
            Self::Deadline => serde_json::json!({ "deadline": { "bound_ms": 1000 } }),
            Self::Wall => serde_json::json!({ "wall": { "bound_ms": 1000 } }),
            Self::Died => serde_json::Value::Null,
        }
    }

    /// The refusal the model reads.
    fn refusal(self) -> &'static str {
        match self {
            Self::Cpu => "CELL_BOUND_EXCEEDED: worker CPU time limit exceeded (limit 1000 ms)",
            Self::Deadline => {
                "CELL_BOUND_EXCEEDED: worker compute deadline exceeded (limit 1000 ms)"
            }
            Self::Wall => "CELL_BOUND_EXCEEDED: worker wall time limit exceeded (limit 1000 ms)",
            Self::Died => "CELL_WORKER_DIED: the worker running the cell died (killed by signal 6)",
        }
    }

    /// What the model is told to change, when the refusal names the cell
    /// as the cause of its worker's death: only a limit lash saw does
    /// (FIG-6806).
    fn advice(self) -> Option<&'static str> {
        match self {
            Self::Died => Some(
                "The cell's run passed a limit of its worker, such as its memory ceiling, and running it again meets it again. Hold less data at once, or split the work across smaller cells.",
            ),
            Self::Cpu | Self::Deadline | Self::Wall => None,
        }
    }
}

/// [`served::rlm`] using the pool's instruction ceiling.
fn unbounded_rlm(workers: lash::vm::WorkerService) -> lash::rlm::RlmProtocolPluginFactory {
    lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::unbounded())
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        lash::rlm::CellDialect::typescript(),
    )
    .with_worker_service(workers)
}

/// The settled output of `session`'s turn of `input`, or the park its
/// session took instead: a parked turn never settles.
async fn settled_or_parked(
    world: &World,
    session: &lash::DurableSession,
    input: &str,
) -> Result<lash::TurnOutput, String> {
    let actor = lash_core::durable_port::ActorKey::session(input).expect("a session actor key");
    let durable = Arc::clone(world.backend.durable());
    let parked = async {
        loop {
            if let Ok(Some(snapshot)) = durable.actor(&actor).await
                && snapshot.state == lash_core::durable_port::ActorState::Parked
            {
                return snapshot.park.unwrap_or_default();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let output = session.send(lash::TurnInput::text(input)).output();
    tokio::time::timeout(served::WATCHDOG, async {
        tokio::select! {
            output = output => Ok(output.expect("the turn answers")),
            park = parked => Err(park),
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deadlock watchdog: the turn of `{input}` never settled"))
}

/// FIG-6523: a cell whose worker meets `limit` is refused with it and its
/// configured value. The same cell meets it again, so its one run is its
/// answer: the next model step reads the refusal, the turn goes on to its
/// answer, and no pass runs the cell again or parks the session. Before,
/// the loss was a retried crash or deadline, pass after pass, until the
/// session parked and the model never saw it.
async fn worker_limit_case(tier: Tier, limit: Limit) {
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.max_workers = 1;
    // One exchange runs the loop until the limit stops it.
    config.tuning.slice = 1 << 50;
    limit.configure(&mut config);
    let workers = lash::vm::WorkerService::new(config).with_worker_receipts();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let model = {
        let requests = Arc::clone(&requests);
        lash_core::testing::TestProvider::builder()
            .kind("worker-limit")
            .complete(move |request: LlmRequest| {
                let mut asked = requests.lock().unwrap();
                asked.push(serde_json::to_string(&request.messages).expect("a request encodes"));
                let response = if asked.len() == 1 {
                    served::cell(limit.cell())
                } else {
                    served::text(&request, "done")
                };
                async move { Ok(response) }
            })
            .build()
            .into_handle()
    };
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        lash::LashCore::rlm_builder(backend.clone(), unbounded_rlm(workers.clone()))
    })
    .await
    else {
        return;
    };
    let input = format!("worker-limit-{limit:?}").to_lowercase();
    let session = world.session(&input, served::spec(8)).await;
    let settled = settled_or_parked(&world, &session, &input).await;
    let runs = workers
        .worker_receipts()
        .iter()
        .filter(|receipt| format!("{:?}", receipt.path) == "Cell")
        .count();
    let output = settled.unwrap_or_else(|park| {
        panic!("the turn parked after {runs} runs of the cell instead of answering: {park}")
    });
    assert_eq!(runs, 1, "no pass runs the refused cell again");
    let asked = requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "the model is asked once after the cell");
    assert!(
        asked[1].contains(limit.refusal()),
        "the next model step reads the refusal: {}",
        asked[1]
    );
    if let Some(advice) = limit.advice() {
        assert!(
            asked[1].contains(advice),
            "the refusal names the cell as the cause: {}",
            asked[1]
        );
    }
    assert_eq!(output.assistant_message(), Some("done"));
    let transcript = session.transcript().await.expect("the transcript decodes");
    let failures: Vec<_> = transcript
        .visible()
        .filter_map(|entry| match &entry.item {
            lash::transcript::TranscriptItem::Cell(cell) => match &cell.result {
                lash::transcript::CellOutcome::Failed(failure) => Some(failure.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 1, "one refused cell: {failures:?}");
    assert_eq!(
        serde_json::to_value(failures[0].worker_limit).expect("a limit encodes"),
        limit.typed(),
        "the refusal carries the typed limit: {:?}",
        failures[0]
    );
    world.shutdown().await;
}

async fn a_cell_past_its_worker_cpu_budget_is_refused_once_and_the_model_reads_it(tier: Tier) {
    worker_limit_case(tier, Limit::Cpu).await;
}

async fn a_cell_past_its_worker_compute_deadline_is_refused_once_and_the_model_reads_it(
    tier: Tier,
) {
    worker_limit_case(tier, Limit::Deadline).await;
}

async fn a_cell_past_its_worker_wall_time_is_refused_once_and_the_model_reads_it(tier: Tier) {
    worker_limit_case(tier, Limit::Wall).await;
}

async fn a_cell_that_kills_its_worker_is_refused_once_and_the_model_reads_it(tier: Tier) {
    worker_limit_case(tier, Limit::Died).await;
}

/// Kills worker `pid` from outside its cell, as an operator, a node drain
/// or a neighbour's out-of-memory kill does: no limit of lash's is in
/// sight. A worker already gone is no failure.
#[expect(
    clippy::disallowed_methods,
    reason = "the law kills a real worker process from outside, as an operator would"
)]
fn kill_from_outside(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("signal the worker");
}

/// `count_call`'s body whose first run kills every worker the service
/// checked out, the one the cell holds across the call among them, and
/// then answers: the call ran, and its worker is gone.
struct KillHeld {
    runs: Arc<AtomicUsize>,
    workers: lash::vm::WorkerService,
    killed: Arc<Mutex<Vec<u32>>>,
}

#[async_trait::async_trait]
impl StaticToolExecute for KillHeld {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut killed = self.killed.lock().unwrap();
            for receipt in self.workers.worker_receipts() {
                if !killed.contains(&receipt.pid) {
                    kill_from_outside(receipt.pid);
                    killed.push(receipt.pid);
                }
            }
        }
        ToolOutcome::ok(serde_json::json!({ "result": "counted" })).into()
    }
}

/// A model that writes `cell`, then answers `done`, recording each request
/// it is asked.
fn cell_then_done(
    cell: &'static str,
    requests: &Arc<Mutex<Vec<String>>>,
) -> lash_core::facade_support::ProviderHandle {
    let requests = Arc::clone(requests);
    lash_core::testing::TestProvider::builder()
        .kind("worker-lost")
        .complete(move |request: LlmRequest| {
            let mut asked = requests.lock().unwrap();
            asked.push(serde_json::to_string(&request.messages).expect("a request encodes"));
            let response = if asked.len() == 1 {
                served::cell(cell)
            } else {
                served::text(&request, "done")
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle()
}

/// FIG-6806: a worker killed from outside while its cell holds it across a
/// tool call is no end of the cell's own. The turn runs the cell again from
/// its journal on a fresh worker: the call's outcome is replayed, never run
/// again, the cell finishes with it, and the model never reads the loss.
/// Since FIG-6786 the first such loss ended the cell with
/// `CELL_WORKER_DIED`, which the model read.
async fn a_worker_killed_from_outside_across_a_held_tool_call_reruns_the_cell_on_a_fresh_worker(
    tier: Tier,
) {
    let workers = lash::vm::WorkerService::default().with_worker_receipts();
    let runs = Arc::new(AtomicUsize::new(0));
    let killed = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        cell_then_done(CELL, &requests),
        |backend| {
            lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers.clone()))
                .tools(
                    lash::tool_provider_key!("tools"),
                    count_call(KillHeld {
                        runs: Arc::clone(&runs),
                        workers: workers.clone(),
                        killed: Arc::clone(&killed),
                    }),
                )
        },
    )
    .await
    else {
        return;
    };
    let input = "worker-lost-across-a-held-call";
    let session = world.session(input, served::spec(8)).await;
    let output = settled_or_parked(&world, &session, input)
        .await
        .unwrap_or_else(|park| panic!("the turn parked instead of answering: {park}"));
    let killed = killed.lock().unwrap().clone();
    assert!(!killed.is_empty(), "the call killed the cell's worker");
    let receipts = workers.worker_receipts();
    assert!(
        receipts.iter().any(
            |receipt| format!("{:?}", receipt.path) == "Cell" && !killed.contains(&receipt.pid)
        ),
        "the cell ran again on a fresh worker: {receipts:?}"
    );
    let asked = requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "the model never reads the loss: {asked:?}");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the held call's effect is not repeated"
    );
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!({ "first": { "result": "counted" } }))
    );
    world.shutdown().await;
}

/// FIG-6806: a cell whose worker is killed from outside on every run, after
/// the run began computing, runs on a fresh worker until its attempts are
/// spent, and then is refused once with `CELL_WORKER_DIED`. The refusal
/// says the worker was lost from outside and that running the cell again
/// is safe, since no call of it ran; it never tells the model to change
/// the cell.
async fn a_cell_whose_worker_is_lost_from_outside_on_every_run_is_refused_without_blaming_the_cell(
    tier: Tier,
) {
    let config = lash::vm::WorkerService::default().config().clone();
    let attempts = usize::try_from(config.deadlines.max_attempts).expect("a small count");
    let receipts = lash::vm::WorkerService::new(config).with_worker_receipts();
    // The worker of the run checked out last, and the calls made on it. A
    // run's first call sends its start with its first slice; its second
    // finds the run computing and its worker killed.
    let seen = Mutex::new((0_u32, 0_usize));
    let workers = receipts.clone().with_call_hold(Arc::new(move || {
        if let Some(last) = receipts.worker_receipts().last()
            && format!("{:?}", last.path) == "Cell"
        {
            let mut seen = seen.lock().unwrap();
            if seen.0 != last.pid {
                *seen = (last.pid, 0);
            }
            seen.1 += 1;
            if seen.1 == 2 {
                kill_from_outside(last.pid);
            }
        }
        Box::new(())
    }));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        cell_then_done(HEAVY, &requests),
        |backend| lash::LashCore::rlm_builder(backend.clone(), unbounded_rlm(workers.clone())),
    )
    .await
    else {
        return;
    };
    let input = "worker-lost-on-every-run";
    let session = world.session(input, served::spec(8)).await;
    let settled = settled_or_parked(&world, &session, input).await;
    let runs = workers
        .worker_receipts()
        .iter()
        .filter(|receipt| format!("{:?}", receipt.path) == "Cell")
        .count();
    let output = settled.unwrap_or_else(|park| {
        panic!("the turn parked after {runs} runs of the cell instead of answering: {park}")
    });
    assert_eq!(
        runs, attempts,
        "each of the cell's attempts ran on a fresh worker"
    );
    let asked = requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "the model is asked once after the cell");
    assert!(
        asked[1].contains(
            "CELL_WORKER_DIED: the worker running the cell was lost from outside the cell (killed by signal 9)"
        ),
        "the next model step reads the loss: {}",
        asked[1]
    );
    assert!(
        asked[1].contains("running the cell again is safe"),
        "the refusal says whether running the cell again is safe: {}",
        asked[1]
    );
    assert!(
        !asked[1].contains("Hold less data"),
        "the refusal does not blame the cell: {}",
        asked[1]
    );
    assert_eq!(output.assistant_message(), Some("done"));
    world.shutdown().await;
}

/// A cell that completes a call and then computes for several slices.
const CALL_THEN_LOOP: &str = "const first = await tools.countCall({}); let n = 0; \
     for (let i = 0; i < 500000; i = i + 1) { n = n + 1; } await control.finish({ first, n });";

/// The ends of the cell's execution a follower of `updates` reads, up to
/// and including its first completed one. A gap is lost continuity, which
/// this follower never accepts.
async fn cell_ends(
    updates: &mut lash::observe::SessionObservationStream,
) -> Vec<lash::tracing::TraceLanguageExecutionStatus> {
    use lash::observe::SessionObservationStreamItem;
    use lash::observe::Stream as _;
    use lash::tracing::{TraceLanguageExecutionPayload, TraceLanguageExecutionStatus};
    let mut ends = Vec::new();
    loop {
        let update = tokio::time::timeout(
            served::WATCHDOG,
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut *updates).poll_next(cx)),
        )
        .await
        .expect("deadlock watchdog: the follower read no end of the cell")
        .expect("the observation stream stays open")
        .expect("the observation stream answers");
        let event = match update {
            SessionObservationStreamItem::Event(event) => event,
            SessionObservationStreamItem::Gap { gap, .. } => {
                panic!("the follower lost continuity after ends {ends:?}: {gap:?}")
            }
        };
        if let lash_core::SessionObservationEventPayload::LanguageExecution(observation) =
            &event.payload
            && let TraceLanguageExecutionPayload::ExecutionFinished { status, .. } =
                &observation.execution.payload
        {
            ends.push(*status);
            if *status == TraceLanguageExecutionStatus::Completed {
                return ends;
            }
        }
    }
}

/// FIG-6891: a worker killed from outside while its cell computes, after
/// the cell's first call completed and its language facts were published,
/// costs a follower nothing. The pass that lost the worker answers nothing,
/// so it publishes no end of the cell: the rerun's end is the cell's only
/// one, every fact the rerun repeats is the fact the lost pass published,
/// and the live replay keeps its incarnation. Before, the lost pass
/// published a failed end and the rerun a completed one under the same
/// key: the store refused the second as a conflicting redelivery and
/// rotated its incarnation, which reset every follower's replay.
async fn a_worker_lost_mid_cell_and_rerun_keeps_live_replay_continuity(tier: Tier) {
    let receipts = lash::vm::WorkerService::default().with_worker_receipts();
    let runs = Arc::new(AtomicUsize::new(0));
    // The cell's worker and the calls it was sent since the call ran: the
    // first hands it the call's outcome and a slice of the loop, and the
    // second finds the loop computing and the worker killed.
    let seen = Mutex::new((None::<u32>, 0_usize));
    let called = Arc::clone(&runs);
    let workers = receipts.clone().with_call_hold(Arc::new(move || {
        if called.load(Ordering::SeqCst) > 0
            && let Some(last) = receipts.worker_receipts().last()
            && format!("{:?}", last.path) == "Cell"
        {
            let mut seen = seen.lock().unwrap();
            match seen.0 {
                None => *seen = (Some(last.pid), 1),
                Some(pid) if pid == last.pid && seen.1 < 2 => {
                    seen.1 += 1;
                    if seen.1 == 2 {
                        kill_from_outside(pid);
                    }
                }
                Some(_) => {}
            }
        }
        Box::new(())
    }));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let Some(world) = World::with_model(
        tier,
        Vec::new(),
        cell_then_done(CALL_THEN_LOOP, &requests),
        |backend| {
            lash::LashCore::rlm_builder(backend.clone(), unbounded_rlm(workers.clone()))
                .tools(lash::tool_provider_key!("tools"), count(&runs))
        },
    )
    .await
    else {
        return;
    };
    let input = "worker-lost-mid-cell-replay";
    let session = world.session(input, served::spec(8)).await;
    let session_id =
        lash_kernel_value::identity::SessionId::try_from(input.to_owned()).expect("a session id");
    let observed = world
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("the session opens for observation");
    let (snapshot, mut updates) = observed
        .observe()
        .follow(lash::FeedStart::Head)
        .await
        .expect("the snapshot reads the durable head");
    let incarnation = snapshot
        .cursor
        .parse()
        .expect("the snapshot cursor parses")
        .replay_incarnation_id
        .to_owned();

    let (settled, ends) = tokio::join!(
        settled_or_parked(&world, &session, input),
        cell_ends(&mut updates),
    );
    let output =
        settled.unwrap_or_else(|park| panic!("the turn parked instead of answering: {park}"));
    let cells = workers
        .worker_receipts()
        .iter()
        .filter(|receipt| format!("{:?}", receipt.path) == "Cell")
        .count();
    assert!(
        cells >= 2,
        "the cell ran again on a fresh worker: {cells} runs"
    );
    assert_eq!(
        ends,
        vec![lash::tracing::TraceLanguageExecutionStatus::Completed],
        "the follower reads one end of the cell, the rerun's"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the call's effect is not repeated"
    );
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!({ "first": { "result": "counted" }, "n": 500000 }))
    );
    let head = world.core.observation_cursor(
        &session_id,
        snapshot.cursor.parse().expect("parses").revision,
    );
    assert_eq!(
        head.parse()
            .expect("the head cursor parses")
            .replay_incarnation_id,
        incarnation,
        "the live replay keeps its incarnation"
    );
    world.shutdown().await;
}

async fn a_lowering_worker_crash_is_never_a_recorded_host_cell_failure(tier: Tier) {
    setup_crash_case(tier, Request::Lower).await;
}

async fn a_cell_start_worker_crash_is_never_a_recorded_host_cell_failure(tier: Tier) {
    setup_crash_case(tier, Request::Cell).await;
}

const RETIRE_TOOL: &str = "retire_pages";

const RETIRE_TOOL_OPERATION: &str = "tools.retire_pages";
/// Cell 0 of `rows` is 0 under the first park's root, which the activation
/// leases; 1 under the second's; 2 under the third's, which retires the
/// second; and 1 again at the fourth park, whose page the worker sealed at
/// the second and names again under that seal's grant.
const RETIRE_CELL: &str = "const rows = []; for (let i = 0; i < 256; i++) { rows.push(0); }
await tools.retirePages({});
rows[0] = 1;
await tools.retirePages({});
rows[0] = 2;
await tools.retirePages({});
rows[0] = 1;
await tools.retirePages({});
await control.finish({ last: rows[0] });";

/// `retire_pages`' body: it runs page retirement until a step finds
/// nothing left to delete, so every retired root is peeled before the
/// cell's next park.
struct RetirePages {
    stores: Arc<std::sync::OnceLock<Arc<dyn lash_execution_contracts::backend::StoreSet>>>,
}

#[async_trait::async_trait]
impl StaticToolExecute for RetirePages {
    async fn execute(&self, _call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let pages = self
            .stores
            .get()
            .expect("the store set is layered before the core runs")
            .durable_store()
            .pages()
            .expect("the durable store publishes heap pages");
        let budget = lash_durable::domain::pages::RetirementBudget { nodes: 4096 };
        while pages.retire_step(budget).await.expect("a retirement step")
            != lash_durable::domain::pages::RetirementProgress::default()
        {}
        ToolOutcome::ok(serde_json::json!({})).into()
    }
}

/// FIG-6629: a cell whose fourth park names a page its second park
/// committed, after the third park retired the second's root and that
/// root was peeled, commits every park on its one worker lease and
/// finishes. The activation leased only its first park's root, so the
/// store refused the fourth park's candidate (`checkpoint graph refused:
/// node … is missing`), and the cell ran again from its third park on a
/// second lease.
async fn a_page_an_earlier_park_committed_stays_stored_while_its_activation_names_it(tier: Tier) {
    let stores = Arc::new(std::sync::OnceLock::new());
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        RETIRE_TOOL,
        RETIRE_TOOL_OPERATION,
        "Retires every page no root holds.",
        serde_json::json!({ "type": "object", "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("retire_pages' schemas")
    .with_execution(std::time::Duration::from_secs(120));
    let tools: Arc<dyn lash_core::ToolProvider> = Arc::new(StaticToolProvider::new(
        vec![definition],
        RetirePages {
            stores: Arc::clone(&stores),
        },
    ));
    let warming = AtomicBool::new(true);
    let model = lash_core::testing::TestProvider::builder()
        .kind("page-retirement")
        .complete(move |_request: LlmRequest| {
            let response = if warming.swap(false, Ordering::SeqCst) {
                served::cell("await control.finish(\"warm\");")
            } else {
                served::cell(RETIRE_CELL)
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle();
    let workers = lash::vm::WorkerService::default();
    let Some(world) = World::layered_with_model(
        tier,
        |layered| {
            assert!(stores.set(Arc::clone(&layered)).is_ok(), "layered once");
            layered
        },
        model,
        |backend| {
            lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers.clone()))
                .tools(lash::tool_provider_key!("tools"), tools)
        },
    )
    .await
    else {
        return;
    };
    // A first cell starts the pool and warms its worker.
    let warm = world.session("page-retirement-warm", served::spec(8)).await;
    world.send(&warm, "warm the worker").await;
    let checkouts = || {
        workers
            .pool()
            .expect("the pool")
            .measurements()
            .counters
            .checkouts
    };
    let before = checkouts();
    let session = world.session("page-retirement", served::spec(8)).await;
    let output = world.send(&session, "retire between parks").await;
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!({ "last": 1 }))
    );
    assert_eq!(
        checkouts() - before,
        1,
        "every park of the cell commits on its one worker lease"
    );
    world.shutdown().await;
}

tiered_laws!(
    a_page_an_earlier_park_committed_stays_stored_while_its_activation_names_it,
    a_lowering_worker_crash_is_never_a_recorded_host_cell_failure,
    a_cell_start_worker_crash_is_never_a_recorded_host_cell_failure,
    a_tool_and_finish_cell_crosses_the_worker_boundary_on_one_lease,
    a_cell_past_its_worker_cpu_budget_is_refused_once_and_the_model_reads_it,
    a_cell_past_its_worker_compute_deadline_is_refused_once_and_the_model_reads_it,
    a_cell_past_its_worker_wall_time_is_refused_once_and_the_model_reads_it,
    a_cell_that_kills_its_worker_is_refused_once_and_the_model_reads_it,
    a_worker_killed_from_outside_across_a_held_tool_call_reruns_the_cell_on_a_fresh_worker,
    a_cell_whose_worker_is_lost_from_outside_on_every_run_is_refused_without_blaming_the_cell,
    a_worker_lost_mid_cell_and_rerun_keeps_live_replay_continuity,
);
