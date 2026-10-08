//! A VM worker's death during a cell's setup fails the attempt, never the
//! cell (FIG-4459), through a host's `send()` with the core's node serving
//! the turn.
//!
//! The pool has one worker. Once the model has answered with a cell, the
//! worker is killed before the cell's first worker request (its source
//! analysis) or its second (its compilation). The request meets the real
//! closed transport: a host verdict, read live outside any recorded step,
//! so the attempt fails retryably and seals nothing. The retried pass runs
//! the cell on a replacement worker: the turn finishes with the cell's own
//! result, the model is asked once and never sees a failure, and the cell's
//! tool runs once.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::rlm::Dialect as _;
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::LlmRequest;
use lash_core::{ExecutionPolicy, ToolCall, ToolOutcome};
use served::{Tier, World};

const TOOL: &str = "count_call";
const CELL: &str = "const first = await tools.count_call({}); finish({ first });";

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
    let definition = lash_core::ToolDefinition::raw(
        TOOL,
        TOOL,
        "Counts its calls.",
        serde_json::json!({ "type": "object", "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("count_call's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL));
    Arc::new(StaticToolProvider::new(
        vec![definition],
        Count {
            runs: Arc::clone(runs),
        },
    ))
}

/// The pool's one worker, killed once before a chosen checkout of the
/// cell's.
struct Killer {
    workers: lash::rlm::WorkerService,
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
    lash_core::testing::TestProvider::builder()
        .kind("worker-setup-crash")
        .complete(move |_request: LlmRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            killer.arm();
            async move { Ok(served::cell(CELL)) }
        })
        .build()
        .into_handle()
}

/// The cell's request the worker dies before: its source analysis, or its
/// compilation.
#[derive(Clone, Copy, Debug)]
enum Request {
    References,
    Compile,
}

impl Request {
    /// How many checkouts the cell makes before this request: it reads its
    /// VM state twice, then analyses its source, then compiles it.
    fn before(self) -> usize {
        match self {
            Self::References => 2,
            Self::Compile => 3,
        }
    }
}

async fn setup_crash_case(tier: Tier, request: Request) {
    let mut config = lash::rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.max_workers = 1;
    let receipts = lash::rlm::WorkerService::new(config).with_worker_receipts();
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
    let Some(world) = World::with_model(tier, Vec::new(), model(&killer, &calls), |backend| {
        lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, None, workers.clone()))
            .tools(count(&runs))
    })
    .await
    else {
        return;
    };
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
    assert!(
        output.result.errors.is_empty(),
        "no cell failures: {:?}",
        output.result.errors
    );
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({ "first": { "result": "counted" } }))
    );
    world.shutdown().await;
}

async fn a_source_analysis_worker_crash_is_never_a_recorded_host_cell_failure(tier: Tier) {
    setup_crash_case(tier, Request::References).await;
}

async fn a_compilation_worker_crash_is_never_a_recorded_host_cell_failure(tier: Tier) {
    setup_crash_case(tier, Request::Compile).await;
}

tiered_laws!(
    a_source_analysis_worker_crash_is_never_a_recorded_host_cell_failure,
    a_compilation_worker_crash_is_never_a_recorded_host_cell_failure,
);
