use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::*;
use crate::TestProcessRegistryWriteExt;
use crate::{
    AbandonRequest, AttachmentStore, LeaseOwnerIdentity, LeaseOwnerLiveness,
    ProcessExecutionEnvRef, ProcessInput, ProcessListFilter, ProcessRegistration, ProcessStarted,
    ProcessStatus, TestLocalProcessRegistry, TriggerStore,
};

mod attachment_owner_tests;

const TEST_PROCESS_EXECUTION_CONCURRENCY: usize = 4;

#[test]
fn process_execution_concurrency_validates_semaphore_bounds() {
    DurableProcessWorkerConfig::validate_process_execution_concurrency(1)
        .expect("one process is a valid execution budget");
    assert!(DurableProcessWorkerConfig::validate_process_execution_concurrency(0).is_err());
    assert!(
        DurableProcessWorkerConfig::validate_process_execution_concurrency(
            tokio::sync::Semaphore::MAX_PERMITS + 1,
        )
        .is_err()
    );
}

#[tokio::test]
async fn dispatcher_unwind_clears_running_latch_and_notifies() {
    let scheduler = Arc::new(ProcessExecutionScheduler::new(
        ProcessExecutionConcurrency::new(1).expect("valid test concurrency"),
    ));
    scheduler.state.lock().await.dispatcher_running = true;
    let task_scheduler = Arc::clone(&scheduler);
    let task = crate::task::spawn(async move {
        let _guard = ProcessExecutionDispatcherGuard::new(task_scheduler);
        panic!("test dispatcher unwind");
    });

    assert!(task.await.expect_err("dispatcher task panics").is_panic());
    tokio::time::timeout(Duration::from_secs(1), scheduler.changed.notified())
        .await
        .expect("unwind cleanup notifies dispatcher waiters");
    assert!(
        !scheduler.state.lock().await.dispatcher_running,
        "a later drive pass must be able to start a replacement dispatcher"
    );
}

struct TestSessionStoreFactory;

#[async_trait::async_trait]
impl SessionStoreFactory for TestSessionStoreFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, String> {
        Ok(Arc::new(InMemorySessionStore::default()))
    }

    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct LateBoundProcessRunHandle {
    worker: OnceLock<DurableProcessWorker>,
    enabled: AtomicBool,
}

impl LateBoundProcessRunHandle {
    async fn enable_and_drive(&self) -> Result<(), PluginError> {
        self.enabled.store(true, Ordering::SeqCst);
        self.worker
            .get()
            .expect("test process worker is bound before execution")
            .drive_pending_processes()
            .await
    }
}

#[async_trait::async_trait]
impl crate::ProcessRunHandle for LateBoundProcessRunHandle {
    async fn claim_and_run_pending(&self) -> Result<(), PluginError> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.worker
            .get()
            .expect("test process worker is bound before execution")
            .drive_pending_processes()
            .await
    }
}

async fn worker_with_engine(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessRunHandle>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessRunHandle>,
    ProcessExecutionEnvRef,
) {
    let raw_registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let driver = crate::ProcessWorkDriver::new(
        Arc::clone(&raw_registry),
        Arc::clone(&run_handle) as Arc<dyn crate::ProcessRunHandle>,
    );
    let registry = driver.process_registry();
    let mut runtime_host = RuntimeHostConfig::in_memory();
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_engine(engine);
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::from_token_limits("test-model", Default::default(), 16_384, None)
            .expect("valid model spec"),
        ..crate::SessionPolicy::default()
    };
    let env_ref = crate::persist_process_execution_env(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            runtime_host,
            Arc::new(TestSessionStoreFactory),
            Arc::clone(&registry),
        )
        .with_session_policy(policy)
        .with_process_execution_concurrency(concurrency)
        .expect("valid test process execution concurrency")
        .with_change_hub(driver.change_hub())
        .with_process_work_driver(driver)
        .with_lease_owner(local_owner("engine-worker", "host-a", "engine-start")),
    );
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    (worker, registry, run_handle, env_ref)
}

fn engine_registration(
    id: impl Into<String>,
    kind: &str,
    env_ref: ProcessExecutionEnvRef,
    payload: serde_json::Value,
) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::Engine {
            kind: kind.to_string(),
            payload,
        },
        RecoveryDisposition::Rerunnable,
        crate::ProcessProvenance::host(),
    )
    .with_execution_env_ref(Some(env_ref))
}

async fn terminal_count(registry: &Arc<dyn ProcessRegistry>) -> usize {
    registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list processes")
        .into_iter()
        .filter(ProcessRecord::is_terminal)
        .count()
}

async fn wait_for_terminal_count(
    registry: &Arc<dyn ProcessRegistry>,
    expected: usize,
    description: &str,
) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while terminal_count(registry).await < expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if result.is_err() {
        let records = registry
            .list_processes(&ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("list timed-out processes");
        panic!(
            "timed out waiting for {description}: {}",
            records
                .iter()
                .map(|record| format!("{}={}", record.id, record.status.label()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn inline_worker(
    registry: Arc<dyn ProcessRegistry>,
    lease_owner: LeaseOwnerIdentity,
) -> DurableProcessWorker {
    inline_worker_with_trigger_store(
        registry,
        lease_owner,
        Arc::new(crate::InMemoryTriggerStore::default()),
    )
}

fn inline_worker_with_trigger_store(
    registry: Arc<dyn ProcessRegistry>,
    lease_owner: LeaseOwnerIdentity,
    trigger_store: Arc<dyn TriggerStore>,
) -> DurableProcessWorker {
    struct InlineSessionStoreFactory;

    #[async_trait::async_trait]
    impl SessionStoreFactory for InlineSessionStoreFactory {
        async fn create_store(
            &self,
            _request: &crate::SessionStoreCreateRequest,
        ) -> Result<Arc<dyn crate::RuntimePersistence>, String> {
            Ok(Arc::new(InMemorySessionStore::default()))
        }

        async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            RuntimeHostConfig::in_memory(),
            Arc::new(InlineSessionStoreFactory),
            registry,
        )
        .with_trigger_store(trigger_store)
        .with_lease_owner(lease_owner),
    )
}

/// A registration with an explicit disposition. Uses an External input as a
/// convenient no-env placeholder; the disposition-driven sweep keys off the
/// declared disposition, not the input kind, so these unit tests exercise the
/// verdict without standing up execution infrastructure.
fn registration_with_disposition(
    id: &str,
    disposition: crate::RecoveryDisposition,
) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        disposition,
        crate::ProcessProvenance::host(),
    )
}

async fn abandoned_evidence(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &str,
) -> crate::AbandonEvidence {
    let record = registry
        .get_process(process_id)
        .await
        .expect("process exists");
    match record.status {
        ProcessStatus::Abandoned {
            await_output: ProcessAwaitOutput::Abandoned { evidence, .. },
        } => *evidence,
        other => panic!("expected an Abandoned terminal, got {other:?}"),
    }
}

fn local_owner(owner_id: &str, host_id: &str, process_start: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity {
        owner_id: owner_id.to_string(),
        incarnation_id: format!("{owner_id}:incarnation"),
        liveness: LeaseOwnerLiveness::local_process_for_test(
            host_id,
            "boot-a",
            std::process::id(),
            process_start,
        ),
    }
}

async fn seed_reserved_trigger_delivery(
    trigger_store: &Arc<dyn TriggerStore>,
) -> crate::TriggerDeliveryReservation {
    let source_type = "ui.button.pressed";
    let source_key =
        crate::empty_trigger_source_key(source_type).expect("empty trigger source key");
    let owner_scope = crate::TriggerOwnerScope::host("recovery-test").unwrap();
    let outcome = trigger_store
        .execute_command(
            "recovery-test-register",
            crate::TriggerCommand::Register {
                owner_scope,
                actor: crate::ProcessOriginator::host_scoped("recovery-test"),
                draft: recovery_test_trigger_draft(source_key.clone()),
            },
        )
        .await
        .expect("execute register")
        .expect("register trigger subscription");
    let crate::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("expected registration receipt")
    };
    let subscription = receipt.record_snapshot;
    let ingress = trigger_store
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            source_type,
            source_key.clone(),
            serde_json::json!({ "button": "Blue" }),
            "button-blue-reconcile",
        ))
        .await
        .expect("ingest trigger occurrence");
    let deliveries = ingress.reservations;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscription.subscription_id
    );
    deliveries[0].clone()
}

fn recovery_test_trigger_draft(source_key: String) -> crate::TriggerSubscriptionDraft {
    crate::TriggerSubscriptionDraft::for_process(
        "recovery-test",
        crate::ProcessExecutionEnvRef::new("process-env:test"),
        "ui.button.pressed",
        source_key,
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({ "target": "reconcile" }),
        },
        crate::ProcessIdentity::new("test-engine"),
    )
    .with_payload_schema(crate::LashSchema::any())
}

async fn process_count(registry: &Arc<dyn ProcessRegistry>, process_id: &str) -> usize {
    registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list processes")
        .into_iter()
        .filter(|record| record.id == process_id)
        .count()
}

async fn await_terminal(registry: &Arc<dyn ProcessRegistry>, process_id: &str) {
    let awaiter = crate::ProcessAwaiter::polling(Arc::clone(registry));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        awaiter.await_terminal(process_id),
    )
    .await
    .expect("recovered process reaches terminal within the sweep")
    .expect("recovered process terminal output");
}

struct BoundaryThenTerminalEngine {
    runs: Arc<AtomicUsize>,
}

struct PausedInfraEngine {
    started: Arc<tokio::sync::Notify>,
    fail: Arc<tokio::sync::Notify>,
}

struct FailOnceArtifactReadEngine {
    reads: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ProcessEngine for FailOnceArtifactReadEngine {
    fn kind(&self) -> &'static str {
        "fail-once-artifact-read"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(crate::ProcessInfraError::new(PluginError::Session(
                "injected transient artifact-store read failure".to_string(),
            )));
        }
        Ok(ProcessAwaitOutput::Success {
            value: serde_json::json!({"process_id": context.registration().id}),
            control: None,
        }
        .into())
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for PausedInfraEngine {
    fn kind(&self) -> &'static str {
        "paused-infra"
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        self.started.notify_one();
        self.fail.notified().await;
        Err(crate::ProcessInfraError::new(PluginError::Session(
            "injected infrastructure failure".to_string(),
        )))
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for BoundaryThenTerminalEngine {
    fn kind(&self) -> &'static str {
        "boundary-test"
    }

    async fn run(
        &self,
        mut context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        let record = context
            .processes()
            .record()
            .await
            .expect("process registry read")
            .expect("process remains registered between segments");
        assert!(!record.is_terminal(), "boundary must not write a terminal");
        if run == 0 {
            assert!(context.take_handover().is_none());
            Ok(crate::ProcessRunOutcome::SegmentBoundary(
                crate::SegmentHandover {
                    reason: crate::BoundaryReason::JournalBudget,
                    program_hash: Some("program-v1".to_string()),
                    engine_state: vec![1, 2, 3],
                },
            ))
        } else {
            assert_eq!(
                context
                    .take_handover()
                    .expect("handover reaches re-entry")
                    .engine_state,
                vec![1, 2, 3]
            );
            Ok(crate::ProcessRunOutcome::Terminal(Box::new(
                ProcessAwaitOutput::Success {
                    value: serde_json::json!({ "segments": 2 }),
                    control: None,
                },
            )))
        }
    }
}

struct ProductionChainState {
    roots: usize,
    root_runs: AtomicUsize,
    roots_ready_to_park: AtomicUsize,
    first_children_started: AtomicUsize,
    active_work: AtomicUsize,
    max_active_work: AtomicUsize,
    all_roots_running: tokio::sync::Notify,
    all_roots_ready_to_park: tokio::sync::Notify,
    run_handle: Arc<LateBoundProcessRunHandle>,
}

struct ProductionChainEngine {
    state: Arc<ProductionChainState>,
}

struct NestedProcessEngine {
    runs: Arc<AtomicUsize>,
}

struct SnapshotRecordingEngine {
    payloads: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl crate::ProcessEngine for SnapshotRecordingEngine {
    fn kind(&self) -> &'static str {
        "snapshot-recording-test"
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        self.payloads
            .lock()
            .expect("recorded payloads")
            .push(payload);
        Ok(crate::ProcessRunOutcome::Terminal(Box::new(
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "recorded": true }),
                control: None,
            },
        )))
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for NestedProcessEngine {
    fn kind(&self) -> &'static str {
        "nested-process-test"
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(crate::ProcessRunOutcome::Terminal(Box::new(
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "nested": "done" }),
                control: None,
            },
        )))
    }
}

struct NestedProcessWaitTool;

impl NestedProcessWaitTool {
    fn definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:await_nested_process",
            "await_nested_process",
            "Start and await a nested test process.",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for NestedProcessWaitTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "await_nested_process").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolResult {
        assert!(
            PROCESS_EXECUTION_PERMIT.try_with(|_| ()).is_ok(),
            "production child turn must inherit the outer process execution permit"
        );
        let process_id = "nested-process";
        let request = crate::ProcessStartRequest::new(
            process_id,
            ProcessInput::Engine {
                kind: "nested-process-test".to_string(),
                payload: serde_json::Value::Null,
            },
            RecoveryDisposition::Rerunnable,
            crate::ProcessOriginator::host(),
        )
        .with_grant(Some(crate::ProcessStartGrant {
            session_scope: crate::SessionScope::new("request-descriptor"),
            descriptor: crate::ProcessHandleDescriptor::new(Some("test"), Some("nested process")),
        }));
        if let Err(err) = call.context.processes().start(request).await {
            return crate::ToolResult::err_fmt(format_args!(
                "failed to start nested process: {err}"
            ));
        }
        match call.context.processes().await_process(process_id).await {
            Ok(ProcessAwaitOutput::Success { .. }) => {
                crate::ToolResult::ok(serde_json::json!({ "nested": "done" }))
            }
            Ok(other) => crate::ToolResult::err_fmt(format_args!(
                "nested process returned non-success output: {other:?}"
            )),
            Err(err) => {
                crate::ToolResult::err_fmt(format_args!("failed to await nested process: {err}"))
            }
        }
    }
}

impl ProductionChainEngine {
    async fn wait_for(counter: &AtomicUsize, expected: usize, notify: &tokio::sync::Notify) {
        while counter.load(Ordering::SeqCst) < expected {
            notify.notified().await;
        }
    }

    fn success(process_id: String) -> crate::ProcessRunOutcome {
        crate::ProcessAwaitOutput::Success {
            value: serde_json::json!({ "completed": process_id }),
            control: None,
        }
        .into()
    }

    fn begin_work(&self) {
        let active = self.state.active_work.fetch_add(1, Ordering::SeqCst) + 1;
        self.state
            .max_active_work
            .fetch_max(active, Ordering::SeqCst);
    }

    fn end_work(&self) {
        self.state.active_work.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for ProductionChainEngine {
    fn kind(&self) -> &'static str {
        "production-chain-test"
    }

    async fn run(
        &self,
        context: crate::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let process_id = context.registration().id.clone();
        let role = payload["role"].as_str().expect("chain role");
        let roots = payload["roots"].as_u64().expect("root count") as usize;
        let nodes = payload["nodes"].as_u64().expect("node count") as usize;
        let nested_wait_task = payload["nested_wait_task"].as_bool().unwrap_or(false);
        let catalog = context.resolved_tool_catalog().expect("tool catalog");
        let processes = context.processes();
        let runtime = context
            .into_runtime_context(catalog)
            .expect("engine runtime context");
        let (runtime, runtime_guard) = runtime.into_parts();

        if role == "launcher" {
            for root in 0..roots {
                let registration = ProcessRegistration::session_start_draft(
                    format!("10-root-{root:03}"),
                    ProcessInput::Engine {
                        kind: self.kind().to_string(),
                        payload: serde_json::json!({
                            "role": "node",
                            "root": root,
                            "level": 0,
                            "roots": roots,
                            "nodes": nodes,
                            "nested_wait_task": nested_wait_task,
                        }),
                    },
                    RecoveryDisposition::Rerunnable,
                );
                let reply = runtime
                    .start_child_process(registration, "test-chain", None)
                    .await;
                assert!(
                    reply.output.is_success(),
                    "production root start failed: {:?}",
                    reply.output
                );
            }
            self.state
                .run_handle
                .enable_and_drive()
                .await
                .expect("drive production roots");
            drop(runtime);
            runtime_guard.shutdown().await;
            return Ok(Self::success(process_id));
        }

        self.begin_work();
        let root = payload["root"].as_u64().expect("root index") as usize;
        let level = payload["level"].as_u64().expect("chain level") as usize;
        if level == 0 {
            let running = self.state.root_runs.fetch_add(1, Ordering::SeqCst) + 1;
            if running == self.state.roots {
                self.state.all_roots_running.notify_waiters();
            }
            Self::wait_for(
                &self.state.root_runs,
                self.state.roots,
                &self.state.all_roots_running,
            )
            .await;
        } else if level == 1 {
            assert_eq!(
                self.state.roots_ready_to_park.load(Ordering::SeqCst),
                self.state.roots,
                "a child ran before every saturated root reached its process wait"
            );
            self.state
                .first_children_started
                .fetch_add(1, Ordering::SeqCst);
        }

        if level + 1 < nodes {
            let child_level = level + 1;
            let child_id = format!("{}-node-{root:03}-{child_level:02}", 20 + child_level);
            let registration = ProcessRegistration::session_start_draft(
                child_id.clone(),
                ProcessInput::Engine {
                    kind: self.kind().to_string(),
                    payload: serde_json::json!({
                        "role": "node",
                        "root": root,
                        "level": child_level,
                        "roots": roots,
                        "nodes": nodes,
                        "nested_wait_task": nested_wait_task,
                    }),
                },
                RecoveryDisposition::Rerunnable,
            );
            let reply = runtime
                .start_child_process(registration, "test-chain", None)
                .await;
            assert!(
                reply.output.is_success(),
                "production child start failed: {:?}",
                reply.output
            );
            if level == 0 {
                let ready = self
                    .state
                    .roots_ready_to_park
                    .fetch_add(1, Ordering::SeqCst)
                    + 1;
                if ready == self.state.roots {
                    self.state.all_roots_ready_to_park.notify_waiters();
                }
                Self::wait_for(
                    &self.state.roots_ready_to_park,
                    self.state.roots,
                    &self.state.all_roots_ready_to_park,
                )
                .await;
            }
            self.end_work();
            if nested_wait_task && level == 0 {
                let processes = processes.clone();
                let wait_child_id = child_id.clone();
                crate::task::spawn(inherit_process_execution_permit(async move {
                    processes.await_terminal(&wait_child_id).await
                }))
                .await
                .expect("nested child-turn task joins")
                .expect("nested child-turn task observes child terminal");
            } else {
                processes
                    .await_terminal(&child_id)
                    .await
                    .expect("parent observes production-started child terminal");
            }
            self.begin_work();
        }
        drop(runtime);
        runtime_guard.shutdown().await;
        self.end_work();
        Ok(Self::success(process_id))
    }
}

async fn run_production_chain(
    concurrency: usize,
    roots: usize,
    nodes: usize,
    nested_wait_task: bool,
) {
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let state = Arc::new(ProductionChainState {
        roots,
        root_runs: AtomicUsize::new(0),
        roots_ready_to_park: AtomicUsize::new(0),
        first_children_started: AtomicUsize::new(0),
        active_work: AtomicUsize::new(0),
        max_active_work: AtomicUsize::new(0),
        all_roots_running: tokio::sync::Notify::new(),
        all_roots_ready_to_park: tokio::sync::Notify::new(),
        run_handle: Arc::clone(&run_handle),
    });
    let engine = Arc::new(ProductionChainEngine {
        state: Arc::clone(&state),
    });
    let (worker, registry, _, env_ref) =
        worker_with_engine(concurrency, engine, Arc::clone(&run_handle)).await;
    registry
        .register_process(engine_registration(
            "00-chain-launcher",
            "production-chain-test",
            env_ref,
            serde_json::json!({
                "role": "launcher",
                "roots": roots,
                "nodes": nodes,
                "nested_wait_task": nested_wait_task,
            }),
        ))
        .await
        .expect("seed chain launcher");
    tokio::time::timeout(Duration::from_secs(10), async {
        worker
            .drive_pending_processes()
            .await
            .expect("drive chain launcher");
        wait_for_terminal_count(
            &registry,
            1 + roots * nodes,
            "production-started process chain",
        )
        .await;
    })
    .await
    .expect("production process chain completes without starvation");
    assert_eq!(state.root_runs.load(Ordering::SeqCst), roots);
    assert!(
        state.max_active_work.load(Ordering::SeqCst) <= concurrency,
        "inline process execution exceeded its configured concurrency"
    );
    if nodes > 1 {
        assert_eq!(state.first_children_started.load(Ordering::SeqCst), roots);
    }
    let records = registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list production chain");
    for record in records
        .iter()
        .filter(|record| record.id != "00-chain-launcher")
    {
        assert!(
            !matches!(
                record.provenance.caused_by,
                Some(crate::CausalRef::Process { .. })
            ),
            "production control path must not manufacture process causal refs: {}",
            record.id
        );
    }
}

#[tokio::test]
async fn saturated_fanout_releases_parked_parents_for_children() {
    run_production_chain(
        TEST_PROCESS_EXECUTION_CONCURRENCY,
        TEST_PROCESS_EXECUTION_CONCURRENCY,
        2,
        false,
    )
    .await;
}

#[tokio::test]
async fn concurrency_one_parent_child_chain_completes() {
    run_production_chain(1, 1, 2, false).await;
}

#[tokio::test]
async fn saturated_depth_three_chain_completes() {
    run_production_chain(1, 1, 3, false).await;
}

#[tokio::test]
async fn managed_child_turn_process_wait_releases_outer_run_permit() {
    // Managed child turns cross a fresh Tokio task stack through the same
    // inherited permit scope used here. The wait must park the outer process's
    // only slot so its production-started child can execute.
    run_production_chain(1, 1, 2, true).await;
}

#[tokio::test]
async fn session_turn_process_child_awaits_nested_process_at_concurrency_one() {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("test")
        .complete(move |_request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let response = match call {
                    0 => crate::llm::types::LlmResponse {
                        parts: vec![crate::llm::types::LlmOutputPart::ToolCall {
                            call_id: "await-nested-call".to_string(),
                            tool_name: "await_nested_process".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..Default::default()
                    },
                    1 => crate::llm::types::LlmResponse {
                        full_text: "child turn complete".to_string(),
                        parts: vec![crate::llm::types::LlmOutputPart::Text {
                            text: "child turn complete".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..Default::default()
                    },
                    other => panic!("unexpected provider call {other}"),
                };
                Ok(response)
            }
        })
        .build()
        .into_handle();
    let nested_runs = Arc::new(AtomicUsize::new(0));
    let nested_engine = Arc::new(NestedProcessEngine {
        runs: Arc::clone(&nested_runs),
    });
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let raw_registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let driver = crate::ProcessWorkDriver::new(
        Arc::clone(&raw_registry),
        Arc::clone(&run_handle) as Arc<dyn crate::ProcessRunHandle>,
    );
    let registry = driver.process_registry();
    let mut runtime_host = RuntimeHostConfig::in_memory();
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_engine(nested_engine);
    runtime_host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider));
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::from_token_limits("test-model", Default::default(), 16_384, None)
            .expect("valid model spec"),
        ..crate::SessionPolicy::default()
    };
    let mut plugin_factories = crate::testing::test_standard_protocol_factories();
    plugin_factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        "nested-process-wait-tool",
        crate::PluginSpec::new().with_tool_provider(Arc::new(NestedProcessWaitTool)),
    )));
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(plugin_factories)),
            runtime_host,
            Arc::new(TestSessionStoreFactory),
            Arc::clone(&registry),
        )
        .with_session_policy(policy.clone())
        .with_process_execution_concurrency(1)
        .expect("valid test process execution concurrency")
        .with_change_hub(driver.change_hub())
        .with_process_work_driver(driver)
        .with_lease_owner(local_owner(
            "session-turn-worker",
            "host-a",
            "session-turn-start",
        )),
    );
    run_handle
        .worker
        .set(worker)
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    let outer_process_id = "outer-session-turn";
    let child_request = crate::SessionCreateRequest::child(
        format!("process-session-turn:{outer_process_id}"),
        crate::SessionStartPoint::Empty,
        policy,
        crate::PluginOptions::default(),
        "nested-wait-test",
    )
    .with_session_id("nested-wait-child");
    registry
        .register_process(ProcessRegistration::new(
            outer_process_id,
            ProcessInput::SessionTurn {
                create_request: Box::new(child_request),
                turn_input: Box::new(crate::TurnInput::text("await nested process")),
                output_contract: crate::ToolOutputContract::Static,
            },
            RecoveryDisposition::Rerunnable,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("register production session-turn process");

    tokio::time::timeout(Duration::from_secs(10), async {
        run_handle
            .enable_and_drive()
            .await
            .expect("drive production session-turn process");
        wait_for_terminal_count(&registry, 2, "session-turn process and its nested process").await;
    })
    .await
    .expect("production SessionTurn path completes without permit starvation");
    let outer = crate::ProcessAwaiter::polling(Arc::clone(&registry))
        .await_terminal(outer_process_id)
        .await
        .expect("outer session-turn process is terminal");
    assert!(
        matches!(outer, ProcessAwaitOutput::Success { .. }),
        "outer session-turn process must succeed: {outer:?}"
    );
    assert_eq!(nested_runs.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn segment_boundary_reenters_in_memory_without_premature_terminal() {
    struct Factory;

    #[async_trait::async_trait]
    impl SessionStoreFactory for Factory {
        async fn create_store(
            &self,
            _request: &crate::SessionStoreCreateRequest,
        ) -> Result<Arc<dyn crate::RuntimePersistence>, String> {
            Ok(Arc::new(InMemorySessionStore::default()))
        }

        async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let mut runtime_host = RuntimeHostConfig::in_memory();
    runtime_host.process_engines =
        crate::ProcessEngineRegistry::new().with_engine(Arc::new(BoundaryThenTerminalEngine {
            runs: Arc::clone(&runs),
        }));
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::from_token_limits("test-model", Default::default(), 16_384, None)
            .expect("valid model spec"),
        ..crate::SessionPolicy::default()
    };
    let env_spec =
        crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone());
    let env_ref = crate::persist_process_execution_env(
        runtime_host.durability.process_env_store.as_ref(),
        &env_spec,
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            runtime_host,
            Arc::new(Factory),
            Arc::clone(&registry),
        )
        .with_session_policy(policy)
        .with_lease_owner(local_owner("segment-worker", "host-a", "start-a")),
    );
    registry
        .register_process(
            ProcessRegistration::new(
                "segmented-process",
                ProcessInput::Engine {
                    kind: "boundary-test".to_string(),
                    payload: serde_json::json!({}),
                },
                RecoveryDisposition::Rerunnable,
                crate::ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register process");

    worker
        .drive_pending_processes()
        .await
        .expect("drive process");
    await_terminal(&registry, "segmented-process").await;
    let final_record = registry
        .get_process("segmented-process")
        .await
        .expect("process exists");
    assert_eq!(runs.load(Ordering::SeqCst), 2, "{:?}", final_record.status);
    assert!(matches!(
        final_record.status,
        ProcessStatus::Completed { .. }
    ));
}

#[tokio::test]
async fn sweep_reconciles_reserved_trigger_delivery_without_process() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let trigger_store: Arc<dyn TriggerStore> = Arc::new(crate::InMemoryTriggerStore::default());
    let delivery = seed_reserved_trigger_delivery(&trigger_store).await;
    assert!(
        registry.get_process(&delivery.process_id).await.is_none(),
        "test starts in the reserve/start crash window"
    );

    let worker = inline_worker_with_trigger_store(
        Arc::clone(&registry),
        local_owner("trigger-worker", "host-a", "claimant-start"),
        Arc::clone(&trigger_store),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");

    let record = registry
        .get_process(&delivery.process_id)
        .await
        .expect("sweep registers missing trigger delivery process");
    assert_eq!(record.id, delivery.process_id);
    assert_eq!(process_count(&registry, &delivery.process_id).await, 1);
    assert!(matches!(
        record.provenance.caused_by,
        Some(crate::CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id: Some(subscription_id),
            subscription_incarnation: Some(subscription_incarnation),
            subscription_revision: Some(subscription_revision),
        }) if occurrence_id == delivery.occurrence.occurrence_id
            && subscription_id == delivery.subscription.subscription_id
            && subscription_incarnation == delivery.subscription.incarnation
            && subscription_revision == delivery.subscription.revision
    ));

    worker
        .drive_pending_processes()
        .await
        .expect("second sweep dispatches");
    assert_eq!(
        process_count(&registry, &delivery.process_id).await,
        1,
        "re-running the sweep must not create a duplicate process row"
    );
}

async fn snapshot_recovery_fixture(
    delete_after_reserve: bool,
) -> (
    Arc<dyn ProcessRegistry>,
    Arc<dyn TriggerStore>,
    crate::TriggerDeliveryReservation,
    Arc<Mutex<Vec<serde_json::Value>>>,
    DurableProcessWorker,
) {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let trigger_store: Arc<dyn TriggerStore> = Arc::new(crate::InMemoryTriggerStore::default());
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let mut runtime_host = RuntimeHostConfig::in_memory();
    runtime_host.process_engines =
        crate::ProcessEngineRegistry::new().with_engine(Arc::new(SnapshotRecordingEngine {
            payloads: Arc::clone(&payloads),
        }));
    let policy = crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::from_token_limits("test-model", Default::default(), 16_384, None)
            .expect("valid model spec"),
        ..crate::SessionPolicy::default()
    };
    let env_ref = crate::persist_process_execution_env(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist snapshot recovery process environment");
    let owner_scope = crate::TriggerOwnerScope::host("snapshot-recovery-test").unwrap();
    let actor = crate::ProcessOriginator::host_scoped("snapshot-recovery-test");
    let source_type = "snapshot.recovery";
    let source_key = crate::empty_trigger_source_key(source_type).unwrap();
    let draft = |version: &str| {
        crate::TriggerSubscriptionDraft::for_process(
            "snapshot-recovery-key",
            env_ref.clone(),
            source_type,
            source_key.clone(),
            ProcessInput::Engine {
                kind: "snapshot-recording-test".to_string(),
                payload: serde_json::json!({ "config": version }),
            },
            crate::ProcessIdentity::new("snapshot-recording-test"),
        )
        .with_payload_schema(crate::LashSchema::any())
    };
    trigger_store
        .execute_command(
            "snapshot-register-v1",
            crate::TriggerCommand::Register {
                owner_scope: owner_scope.clone(),
                actor: actor.clone(),
                draft: draft("v1"),
            },
        )
        .await
        .expect("register v1 command")
        .expect("register v1 subscription");
    let delivery = trigger_store
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            source_type,
            source_key.clone(),
            serde_json::json!({ "event": "reserved" }),
            if delete_after_reserve {
                "snapshot-delete-occurrence"
            } else {
                "snapshot-update-occurrence"
            },
        ))
        .await
        .expect("reserve v1 delivery")
        .reservations
        .into_iter()
        .next()
        .expect("one reserved v1 delivery");
    let mutation = if delete_after_reserve {
        crate::TriggerCommand::Delete {
            owner_scope,
            actor,
            subscription_key: "snapshot-recovery-key".to_string(),
            expected_revision: 1,
        }
    } else {
        crate::TriggerCommand::Update {
            owner_scope,
            actor,
            subscription_key: "snapshot-recovery-key".to_string(),
            draft: draft("v2"),
            expected_revision: 1,
        }
    };
    trigger_store
        .execute_command("snapshot-post-reserve-mutation", mutation)
        .await
        .expect("post-reserve mutation command")
        .expect("post-reserve mutation");
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(Vec::new())),
            runtime_host,
            Arc::new(TestSessionStoreFactory),
            Arc::clone(&registry),
        )
        .with_session_policy(policy)
        .with_trigger_store(Arc::clone(&trigger_store))
        .with_lease_owner(local_owner(
            "snapshot-recovery-worker",
            "host-a",
            "snapshot-recovery-start",
        )),
    );
    (registry, trigger_store, delivery, payloads, worker)
}

#[tokio::test]
async fn sweep_recovers_reserved_v1_snapshot_after_v2_update_exactly_once() {
    let (registry, _trigger_store, delivery, payloads, worker) =
        snapshot_recovery_fixture(false).await;

    worker.drive_pending_processes().await.expect("recover v1");
    await_terminal(&registry, &delivery.process_id).await;
    let terminal = registry
        .get_process(&delivery.process_id)
        .await
        .expect("recovered delivery process");
    assert!(
        matches!(terminal.status, ProcessStatus::Completed { .. }),
        "recovered delivery must complete: {:?}",
        terminal.status
    );
    worker
        .drive_pending_processes()
        .await
        .expect("repeat recovery sweep");

    assert_eq!(delivery.subscription.revision, 1);
    assert_eq!(
        payloads.lock().expect("recorded payloads").as_slice(),
        [serde_json::json!({ "args": {}, "config": "v1" })]
    );
}

#[tokio::test]
async fn sweep_recovers_reserved_v1_snapshot_after_tombstone_exactly_once() {
    let (registry, trigger_store, delivery, payloads, worker) =
        snapshot_recovery_fixture(true).await;
    assert!(
        trigger_store
            .list_subscriptions(crate::TriggerSubscriptionFilter::default())
            .await
            .expect("list live subscriptions")
            .is_empty(),
        "the live subscription is tombstoned before recovery"
    );

    worker.drive_pending_processes().await.expect("recover v1");
    await_terminal(&registry, &delivery.process_id).await;
    let terminal = registry
        .get_process(&delivery.process_id)
        .await
        .expect("recovered delivery process");
    assert!(
        matches!(terminal.status, ProcessStatus::Completed { .. }),
        "recovered delivery must complete: {:?}",
        terminal.status
    );
    worker
        .drive_pending_processes()
        .await
        .expect("repeat recovery sweep");

    assert_eq!(delivery.subscription.revision, 1);
    assert_eq!(
        payloads.lock().expect("recorded payloads").as_slice(),
        [serde_json::json!({ "args": {}, "config": "v1" })]
    );
}

#[tokio::test]
async fn sweep_does_not_reconcile_trigger_delivery_pruned_with_terminal_process() {
    let trigger_store = Arc::new(crate::InMemoryTriggerStore::default());
    let registry: Arc<dyn ProcessRegistry> = Arc::new(
        TestLocalProcessRegistry::default().with_trigger_store(Arc::clone(&trigger_store)),
    );
    let trigger_store_dyn: Arc<dyn TriggerStore> = trigger_store.clone();
    let delivery = seed_reserved_trigger_delivery(&trigger_store_dyn).await;
    assert!(
        registry.get_process(&delivery.process_id).await.is_none(),
        "test starts in the reserve/start crash window"
    );

    let worker = inline_worker_with_trigger_store(
        Arc::clone(&registry),
        local_owner("trigger-worker", "host-a", "claimant-start"),
        Arc::clone(&trigger_store_dyn),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    registry
        .get_process(&delivery.process_id)
        .await
        .expect("sweep registers missing trigger delivery process");

    let terminal = registry
        .complete_process(
            &delivery.process_id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "done": true }),
                control: None,
            },
            crate::ProcessCompletionAuthority::workflow_key(&delivery.process_id),
        )
        .await
        .expect("complete trigger delivery process");
    let report = registry
        .prune_terminal_processes(terminal.updated_at_ms.saturating_add(1), None, None)
        .await
        .expect("prune completed trigger delivery process");
    assert_eq!(report.pruned_processes, 1);
    assert!(
        registry.get_process(&delivery.process_id).await.is_none(),
        "terminal trigger delivery process is pruned"
    );
    assert!(
        trigger_store
            .list_deliveries_by_process_id(&delivery.process_id)
            .await
            .expect("list trigger deliveries after prune")
            .is_empty(),
        "prune removes the delivery row together with the process"
    );
    let replayed_registration = trigger_store
        .execute_command(
            "recovery-test-register",
            crate::TriggerCommand::Register {
                owner_scope: crate::TriggerOwnerScope::host("recovery-test").unwrap(),
                actor: crate::ProcessOriginator::host_scoped("recovery-test"),
                draft: recovery_test_trigger_draft(delivery.subscription.source_key.clone()),
            },
        )
        .await
        .expect("retry registration after retention")
        .expect("registration remains valid");
    assert!(matches!(
        replayed_registration,
        crate::TriggerCommandOutcome::Mutation { receipt }
            if receipt.disposition == crate::TriggerMutationDisposition::Unchanged
    ));

    worker
        .drive_pending_processes()
        .await
        .expect("post-prune sweep dispatches");
    assert_eq!(
        process_count(&registry, &delivery.process_id).await,
        0,
        "recovery sweep must not resurrect a delivery whose terminal process was pruned"
    );
}

#[tokio::test]
async fn sweep_does_not_reconcile_trigger_delivery_when_process_exists() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let trigger_store: Arc<dyn TriggerStore> = Arc::new(crate::InMemoryTriggerStore::default());
    let delivery = seed_reserved_trigger_delivery(&trigger_store).await;
    registry
        .register_process(ProcessRegistration::new(
            delivery.process_id.clone(),
            ProcessInput::External {
                metadata: serde_json::json!({ "already": "registered" }),
            },
            RecoveryDisposition::Rerunnable,
            crate::ProcessProvenance::host(),
        ))
        .await
        .expect("pre-register delivery process");

    let worker = inline_worker_with_trigger_store(
        Arc::clone(&registry),
        local_owner("trigger-worker", "host-a", "claimant-start"),
        trigger_store,
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");

    let record = registry
        .get_process(&delivery.process_id)
        .await
        .expect("existing process remains");
    assert_eq!(record.provenance.caused_by, None);
    assert_eq!(
        process_count(&registry, &delivery.process_id).await,
        1,
        "existing process row must be treated as already started"
    );
}

/// ExternallyOwned rows are never claimed and never run: lash does not own
/// their execution (ADR 0019).
#[tokio::test]
async fn sweep_never_claims_externally_owned_rows() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ext",
            RecoveryDisposition::ExternallyOwned,
        ))
        .await
        .expect("register");

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let record = registry.get_process("proc-ext").await.expect("process");
    assert!(
        !record.is_terminal(),
        "an externally-owned row must never be claimed or run by the sweep"
    );
    assert!(
        registry
            .get_process_lease("proc-ext")
            .await
            .expect("lease read")
            .is_none(),
        "the sweep must not claim a lease on an externally-owned row"
    );
}

/// A pending Abandon Request on an externally-owned row is reconciled into
/// `Abandoned{reconciled_request}` — there is no owner lease to wait out.
#[tokio::test]
async fn sweep_reconciles_externally_owned_abandon_request() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ext-abandon",
            RecoveryDisposition::ExternallyOwned,
        ))
        .await
        .expect("register");
    registry
        .request_process_abandon(
            "proc-ext-abandon",
            AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 1,
                reason: Some("host retired".to_string()),
            },
        )
        .await
        .expect("request abandon");

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    await_terminal(&registry, "proc-ext-abandon").await;

    let evidence = abandoned_evidence(&registry, "proc-ext-abandon").await;
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert!(
        evidence.owner.is_none(),
        "externally-owned work names no lash execution owner"
    );
}

/// A started OwnerBound row whose holder is provably dead is terminalized as
/// `Abandoned{sweep}`, never re-run — replacing a re-execution would repeat
/// non-idempotent side effects.
#[tokio::test]
async fn sweep_abandons_started_owner_bound_with_provably_dead_holder() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ob-dead",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register");
    let dead_holder = local_owner("dead-worker", "host-a", "not-the-current-process-start");
    registry
        .record_first_started(
            "proc-ob-dead",
            ProcessStarted {
                owner: dead_holder.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record started");
    registry
        .claim_process_lease("proc-ob-dead", &dead_holder, 60_000)
        .await
        .expect("dead holder claims")
        .acquired()
        .expect("dead holder lease acquired");

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    await_terminal(&registry, "proc-ob-dead").await;

    let evidence = abandoned_evidence(&registry, "proc-ob-dead").await;
    assert_eq!(evidence.writer, AbandonWriter::Sweep);
    assert_eq!(
        evidence.owner.as_ref().map(|owner| owner.owner_id.as_str()),
        Some("dead-worker"),
        "the sweep names the provably-dead holder as the abandoned owner"
    );

    // Revenant: the dead owner reappears and tries to complete the row. The
    // terminal append adopts the stored outcome rather than overwriting it.
    let replayed = registry
        .complete_process(
            "proc-ob-dead",
            ProcessAwaitOutput::Success {
                value: serde_json::json!("revenant"),
                control: None,
            },
            crate::ProcessCompletionAuthority::workflow_key("proc-ob-dead"),
        )
        .await
        .expect("terminal retry adopts stored outcome");
    assert!(matches!(replayed.status, ProcessStatus::Abandoned { .. }));
}

/// A started OwnerBound row whose holder is merely silent (no death evidence)
/// and carries no Abandon Request is left non-terminal — elapsed time alone
/// never terminalizes.
#[tokio::test]
async fn sweep_skips_started_owner_bound_with_silent_holder() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ob-silent",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register");
    registry
        .record_first_started(
            "proc-ob-silent",
            ProcessStarted {
                owner: LeaseOwnerIdentity::opaque("started-worker", "started-incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record started");
    // Opaque live holder: no liveness proof, so it is never provably dead.
    registry
        .claim_process_lease(
            "proc-ob-silent",
            &LeaseOwnerIdentity::opaque("other-worker", "other-incarnation"),
            60_000,
        )
        .await
        .expect("live holder claims")
        .acquired()
        .expect("live holder lease acquired");

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let record = registry
        .get_process("proc-ob-silent")
        .await
        .expect("process");
    assert!(
        !record.is_terminal(),
        "a silent, not-provably-dead holder with no abandon request stays non-terminal"
    );
}

/// A started OwnerBound row with a lapsed lease and a pending Abandon Request
/// is reconciled into `Abandoned{reconciled_request}`, naming the started
/// owner as the lapsed owner.
#[tokio::test]
async fn sweep_reconciles_started_owner_bound_after_lease_lapse() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ob-lapse",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register");
    registry
        .record_first_started(
            "proc-ob-lapse",
            ProcessStarted {
                owner: LeaseOwnerIdentity::opaque("lapsed-owner", "lapsed-incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record started");
    registry
        .request_process_abandon(
            "proc-ob-lapse",
            AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 2,
                reason: None,
            },
        )
        .await
        .expect("request abandon");
    // No live lease held: the row's owner lease has lapsed.

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    await_terminal(&registry, "proc-ob-lapse").await;

    let evidence = abandoned_evidence(&registry, "proc-ob-lapse").await;
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert_eq!(
        evidence.owner.as_ref().map(|owner| owner.owner_id.as_str()),
        Some("lapsed-owner"),
        "the reconciled abandonment names the started owner as the lapsed owner"
    );
}

/// An OwnerBound row that has never started is claimable and runnable by any
/// worker (first execution is not re-execution): the runner records
/// `first_started`. If execution infrastructure is unavailable, the row stays
/// non-terminal and becomes claimable again rather than recording a failure.
#[tokio::test]
async fn owner_bound_unstarted_infra_failure_stays_claimable() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(registration_with_disposition(
            "proc-ob-unstarted",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register");

    let worker = inline_worker(
        Arc::clone(&registry),
        local_owner("live-worker", "host-a", "claimant-start"),
    );
    worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry
                .get_process("proc-ob-unstarted")
                .await
                .expect("process")
                .first_started
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runner records first_started before infrastructure fails");
    let next_owner = local_owner("next-worker", "host-b", "claimant-next");
    let reclaimed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match registry
                .claim_process_lease("proc-ob-unstarted", &next_owner, 60_000)
                .await
                .expect("claim after infrastructure failure")
            {
                crate::ProcessLeaseClaimOutcome::Acquired(lease) => break lease,
                crate::ProcessLeaseClaimOutcome::Busy { .. } => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("infrastructure failure releases its lease");
    let record = registry
        .get_process("proc-ob-unstarted")
        .await
        .expect("process");
    assert!(
        record.first_started.is_some(),
        "the runner must record first_started before executing an unstarted OwnerBound row"
    );
    assert!(
        !record.is_terminal(),
        "infrastructure failure must not write a terminal, got {:?}",
        record.status
    );
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&reclaimed))
        .await
        .expect("release verification claim");
}

#[tokio::test]
async fn missing_engine_configuration_is_retryable_infrastructure_failure() {
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(SnapshotRecordingEngine {
            payloads: Arc::new(Mutex::new(Vec::new())),
        }),
        run_handle,
    )
    .await;
    registry
        .register_process(engine_registration(
            "missing-engine",
            "not-installed",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register missing engine row");
    run_handle
        .enable_and_drive()
        .await
        .expect("drive missing engine row");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let record = registry
                .get_process("missing-engine")
                .await
                .expect("missing engine row");
            let lease = registry
                .get_process_lease("missing-engine")
                .await
                .expect("lease read");
            if record.first_started.is_some() && lease.is_none() {
                assert!(!record.is_terminal());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("infrastructure failure leaves row claimable");

    let next = registry
        .claim_process_lease(
            "missing-engine",
            &local_owner("next-worker", "host-b", "next-start"),
            60_000,
        )
        .await
        .expect("subsequent claim")
        .acquired()
        .expect("row remains claimable");
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&next))
        .await
        .expect("release verification lease");
    drop(worker);
}

#[tokio::test]
async fn transient_engine_artifact_read_retries_and_terminally_commits() {
    let reads = Arc::new(AtomicUsize::new(0));
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (_worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(FailOnceArtifactReadEngine {
            reads: Arc::clone(&reads),
        }),
        run_handle,
    )
    .await;
    registry
        .register_process(engine_registration(
            "artifact-read-retry",
            "fail-once-artifact-read",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register fail-once engine row");
    run_handle
        .enable_and_drive()
        .await
        .expect("drive failing artifact read");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let record = registry
                .get_process("artifact-read-retry")
                .await
                .expect("artifact retry row");
            if record.first_started.is_some()
                && !record.is_terminal()
                && registry
                    .get_process_lease("artifact-read-retry")
                    .await
                    .expect("lease read")
                    .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("transient engine error releases its claim");

    run_handle
        .enable_and_drive()
        .await
        .expect("drive retry after artifact recovery");
    await_terminal(&registry, "artifact-read-retry").await;
    let record = registry
        .get_process("artifact-read-retry")
        .await
        .expect("terminal artifact retry row");
    assert!(record.is_terminal());
    assert_eq!(
        record
            .first_started
            .as_deref()
            .map(|started| started.attempt),
        Some(2)
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

/// Owner drain (ADR 0019): a host closing gracefully terminalizes its own
/// started OwnerBound work inline as `Abandoned{OwnerDrain}` under a live lease,
/// while leaving rerunnable, not-yet-started, and other-owner rows untouched.
#[tokio::test]
async fn drain_terminalizes_this_hosts_started_owner_bound_work() {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(TestLocalProcessRegistry::default());
    let owner = local_owner("drain-host", "host-a", "start-a");
    let worker = inline_worker(Arc::clone(&registry), owner.clone());

    // (a) OwnerBound row this worker started -> drained.
    registry
        .register_process(registration_with_disposition(
            "mine-started",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register mine-started");
    registry
        .record_first_started(
            "mine-started",
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record first_started for mine-started");

    // (b) OwnerBound row a DIFFERENT owner started -> not ours to drain.
    registry
        .register_process(registration_with_disposition(
            "theirs-started",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register theirs-started");
    registry
        .record_first_started(
            "theirs-started",
            ProcessStarted {
                owner: local_owner("other-host", "host-b", "start-b"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record first_started for theirs-started");

    // (c) OwnerBound row never started -> still claimable by anyone.
    registry
        .register_process(registration_with_disposition(
            "mine-unstarted",
            RecoveryDisposition::OwnerBound,
        ))
        .await
        .expect("register mine-unstarted");

    // (d) Rerunnable in-flight row this worker started -> left non-terminal for
    // the next worker (its contract; drain never terminalizes rerunnable work).
    registry
        .register_process(registration_with_disposition(
            "rerunnable",
            RecoveryDisposition::Rerunnable,
        ))
        .await
        .expect("register rerunnable");
    registry
        .record_first_started(
            "rerunnable",
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record first_started for rerunnable");

    let report = worker.drain_owner_bound_work().await.expect("drain");
    assert_eq!(report.abandoned, vec!["mine-started".to_string()]);

    let evidence = abandoned_evidence(&registry, "mine-started").await;
    assert_eq!(evidence.writer, AbandonWriter::OwnerDrain);
    assert_eq!(evidence.owner.as_ref(), Some(&owner));

    for untouched in ["theirs-started", "mine-unstarted", "rerunnable"] {
        assert!(
            !registry
                .get_process(untouched)
                .await
                .expect("row exists")
                .is_terminal(),
            "{untouched} must be left non-terminal by owner drain",
        );
    }
}

#[tokio::test]
async fn inline_start_records_stable_owner_that_owner_drain_can_match() {
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let run_handle = Arc::new(LateBoundProcessRunHandle::default());
    let (worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(PausedInfraEngine {
            started: Arc::clone(&started),
            fail: Arc::clone(&fail),
        }),
        run_handle,
    )
    .await;
    let mut registration = engine_registration(
        "real-inline-owner-bound",
        "paused-infra",
        env_ref,
        serde_json::Value::Null,
    );
    registration.disposition = RecoveryDisposition::OwnerBound;
    registry
        .register_process(registration)
        .await
        .expect("register owner-bound engine");
    run_handle
        .enable_and_drive()
        .await
        .expect("drive owner-bound engine");
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("engine starts");

    let record = registry
        .get_process("real-inline-owner-bound")
        .await
        .expect("started record");
    assert_eq!(
        record.first_started.as_deref().map(|start| &start.owner),
        Some(&worker.config().lease_owner),
        "durable start fact uses the stable worker owner"
    );

    fail.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while registry
            .get_process_lease("real-inline-owner-bound")
            .await
            .expect("lease read")
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("infrastructure failure releases the execution lease");

    let report = worker.drain_owner_bound_work().await.expect("owner drain");
    assert_eq!(
        report.abandoned,
        vec!["real-inline-owner-bound".to_string()]
    );
}
