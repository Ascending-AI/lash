//! A setup worker's death fails the attempt, never the cell (FIG-4459).

use super::*;
use lash_core::store::worker_recovery::{
    WorkerRecoveryClaim, WorkerRecoveryError, WorkerRecoveryLimits, WorkerRecoveryStore,
    WorkerRecoveryTotals,
};
use lash_protocol_rlm::Dialect as _;

/// Kill the pool's sole idle worker before source analysis or compilation.
/// The next request observes the real closed transport, not a mocked error.
struct CrashesFirstSetup {
    inner: Arc<dyn WorkerRecoveryStore>,
    workers: lash::rlm::WorkerService,
    before_request: usize,
    requests: AtomicUsize,
    killed: Mutex<Option<(String, u32)>>,
    reservations: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl WorkerRecoveryStore for CrashesFirstSetup {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        let claim = self.inner.reserve(scope, limits).await?;
        self.reservations.lock().unwrap().push(scope.to_owned());
        Ok(claim)
    }

    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        self.inner.mark_running(claim).await?;
        if self.requests.fetch_add(1, Ordering::SeqCst) == self.before_request {
            let pid = self
                .workers
                .worker_receipts()
                .last()
                .expect("session setup already used the sole worker")
                .pid;
            assert!(
                std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status()
                    .expect("signal the setup worker")
                    .success(),
                "kill the worker before its next setup request"
            );
            *self.killed.lock().unwrap() = Some((claim.scope.clone(), pid));
        }
        Ok(())
    }

    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError> {
        self.inner.settle(claim, totals).await
    }
}

async fn setup_crash_case(engine: Engine, before_request: usize) {
    let mut config = lash_protocol_rlm::TypescriptDialect
        .worker_service()
        .config()
        .clone();
    config.max_workers = 1;
    let workers = lash::rlm::WorkerService::new(config).with_worker_receipts();
    let accounting = Arc::new(CrashesFirstSetup {
        inner: engine.lash_backend().worker_recovery(),
        workers: workers.clone(),
        before_request,
        requests: AtomicUsize::new(0),
        killed: Mutex::new(None),
        reservations: Mutex::new(Vec::new()),
    });
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine.lash_backend())
        .map_worker_recovery({
            let accounting = Arc::clone(&accounting);
            move |_| accounting as Arc<dyn WorkerRecoveryStore>
        })
        .into_backend();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("worker-setup-crash")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, LlmTransportError>(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "<typescript>const first = await tools.count_call({}); finish({ first });</typescript>".into(),
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
    )
    .with_worker_service(workers.clone());
    let executions = Arc::new(AtomicUsize::new(0));
    let core = lash::LashCore::rlm_builder(backend, rlm)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_model(provider, model_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(&executions),
            output: json!({"result": "counted"}),
        }) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "worker-setup-crash",
            "test",
        ))
        .expect("build the lash core");
    let session = created_session(&core, run_tag("worker-setup-crash"))
        .await
        .open()
        .await
        .expect("open the session");
    let output = tokio::time::timeout(
        BOUND,
        session.send(lash::TurnInput::text("count once")).output(),
    )
    .await
    .expect("the setup crash retries and the turn completes")
    .expect("the turn succeeds");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the model never sees a recorded Host failure from the setup crash"
    );
    assert_eq!(
        output.final_value(),
        Some(&json!({"first": {"result": "counted"}}))
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the cell's tool runs once"
    );
    assert!(
        output.result.errors.is_empty(),
        "no cell failures: {:?}",
        output.result.errors
    );
    let (scope, killed_pid) = accounting
        .killed
        .lock()
        .unwrap()
        .clone()
        .expect("the worker was killed");
    assert!(
        accounting
            .reservations
            .lock()
            .unwrap()
            .iter()
            .filter(|reserved| **reserved == scope)
            .count()
            >= 2,
        "the attempt retries the same cell scope"
    );
    assert!(
        workers
            .worker_receipts()
            .iter()
            .any(|receipt| receipt.pid != killed_pid),
        "a replacement worker completes the cell"
    );
    Box::pin(session.close()).await.expect("close the session");
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_analysis_worker_crash_is_never_a_recorded_host_cell_failure() {
    setup_crash_case(Engine::double(0x4459, None).await, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compilation_worker_crash_is_never_a_recorded_host_cell_failure() {
    setup_crash_case(Engine::double(0x4459, None).await, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_source_analysis_worker_crash_is_never_a_recorded_host_cell_failure() {
    setup_crash_case(Engine::live("worker-setup-crash", None).await, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_compilation_worker_crash_is_never_a_recorded_host_cell_failure() {
    setup_crash_case(Engine::live("worker-setup-crash", None).await, 1).await;
}
