//! Deterministic simulator scenarios for the native substrate's typed pacing
//! and process-work admission contracts.
//!
//! `QueuedIngress` in the generated world means pending user turn input. These
//! scenarios deliberately use `native-substrate` and `process-admission` in
//! their ids so durable background work cannot be confused with that ingress.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use lash_core::facade_support::{
    CommitBudget, DurableProcessWorker, DurableProcessWorkerConfig, InMemorySessionStoreFactory,
    PluginHost, ProcessAdmissionIntake, ProcessEngineRegistry, ProcessRecoveryOperation,
    ProcessWorkerFault, QueuedWorkBatchingConfig, RuntimeHostConfig, watch_process_registry,
};
use lash_core::sync::MutexExt as _;
use lash_core::{
    LeaseOwnerIdentity, NativeProcessWork, NativeSubstrateConfig, NoQueuedWork, ProcessAwaitOutput,
    ProcessEngine, ProcessEngineRunContext, ProcessInfraError, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessRegistration, ProcessRegistry, ProcessRunOutcome,
    ProcessStatus, ProcessWorkSubstrate, RecoveryContract, SessionPolicy, TestLocalProcessRegistry,
    ToolCallOutput, TurnBudget, WorkCadencePolicy, WorkerProcessWork, WorkerSweepPolicy,
};
use serde_json::{Value, json};

use crate::scheduler::{BoundaryEvent, BoundaryKind, BoundaryScheduler, DeliveredBoundary};

const HELD_PROCESS_ID: &str = "native-process-admission-a-held-faulted";
const TERMINAL_PROCESS_ID: &str = "native-process-admission-b-terminal-by-peer";
const LEASED_PROCESS_ID: &str = "native-process-admission-c-peer-leased";
const PROCESS_LEASE_TTL_MS: u64 = 60_000;

fn non_default_work_cadence() -> WorkCadencePolicy {
    WorkCadencePolicy {
        retry_initial: Duration::from_millis(7),
        retry_max: Duration::from_millis(41),
        max_transient_attempts: NonZeroU32::new(5).expect("non-zero transient attempts"),
        slow_wake_threshold: Duration::from_millis(13),
        poll_initial: Duration::from_millis(11),
        poll_max: Duration::from_millis(47),
        delivery_batch: NonZeroUsize::new(3).expect("non-zero delivery batch"),
        delivery_retry_initial: Duration::from_millis(17),
        delivery_retry_max: Duration::from_millis(53),
    }
}

fn non_default_native_substrate() -> NativeSubstrateConfig {
    NativeSubstrateConfig {
        worker_sweep: WorkerSweepPolicy {
            intake_page: NonZeroUsize::new(3).expect("non-zero intake page"),
            fetch_attempts: NonZeroUsize::new(2).expect("non-zero fetch attempts"),
            fetch_retry_base: Duration::from_millis(3),
        },
        work_cadence: non_default_work_cadence(),
    }
}

fn pacing_boundaries() -> Vec<BoundaryEvent> {
    [
        "valid-non-default",
        "slow-wake-one-millisecond",
        "slow-wake-sub-millisecond",
        "retry-initial-exceeds-max",
        "zero-worker-fetch-retry",
    ]
    .into_iter()
    .map(|case| {
        BoundaryEvent::new(
            format!("native-substrate:pacing:{case}"),
            "native-substrate-pacing",
            BoundaryKind::LeaseTime,
            1,
            "native-substrate.pacing.validate",
            json!({"case": case}),
        )
    })
    .collect()
}

fn observe_pacing_boundary(event: &BoundaryEvent) -> Value {
    let case = event.payload["case"]
        .as_str()
        .expect("pacing boundary carries a case");
    let mut config = non_default_native_substrate();
    let expected_valid = match case {
        "valid-non-default" => true,
        "slow-wake-one-millisecond" => {
            config.work_cadence.slow_wake_threshold = Duration::from_millis(1);
            true
        }
        "slow-wake-sub-millisecond" => {
            config.work_cadence.slow_wake_threshold = Duration::from_micros(999);
            false
        }
        "retry-initial-exceeds-max" => {
            config.work_cadence.retry_initial = Duration::from_millis(42);
            config.work_cadence.retry_max = Duration::from_millis(41);
            false
        }
        "zero-worker-fetch-retry" => {
            config.worker_sweep.fetch_retry_base = Duration::ZERO;
            false
        }
        unexpected => panic!("unknown pacing scenario `{unexpected}`"),
    };
    let validation = config.validate();
    assert_eq!(
        validation.is_ok(),
        expected_valid,
        "pacing case `{case}` returned {validation:?}"
    );
    json!({
        "case": case,
        "valid": validation.is_ok(),
        "slow_wake_threshold_micros": config.work_cadence.slow_wake_threshold.as_micros(),
        "worker_fetch_retry_micros": config.worker_sweep.fetch_retry_base.as_micros(),
        "non_default": true,
    })
}

fn run_pacing_scenario(seed: u64) -> Vec<DeliveredBoundary> {
    let mut scheduler = BoundaryScheduler::with_events(seed, pacing_boundaries());
    let mut delivered = Vec::new();
    while let Some(event) = scheduler.deliver_next_with(observe_pacing_boundary) {
        delivered.push(event);
    }
    delivered
}

struct HeldSuccessEngine {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    runs: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ProcessEngine for HeldSuccessEngine {
    fn kind(&self) -> &'static str {
        "sim-native-process-admission"
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        _payload: Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        self.runs
            .lock_recover()
            .push(context.registration().id.clone());
        self.started.notify_one();
        let _permit = self
            .release
            .acquire()
            .await
            .expect("native process admission release permit");
        Ok(
            ProcessAwaitOutput::from_tool_output(ToolCallOutput::success(json!({
                "process_id": context.registration().id,
                "engine": "sim-native-process-admission",
            })))
            .into(),
        )
    }
}

#[derive(Clone, Default)]
struct AdmissionFaultSink {
    faults: Arc<std::sync::Mutex<Vec<ProcessWorkerFault>>>,
    changed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for AdmissionFaultSink {
    async fn emit(&self, _event: &lash_core::ProcessEvent) {}

    async fn emit_worker_fault(&self, fault: &ProcessWorkerFault) {
        self.faults.lock_recover().push(fault.clone());
        self.changed.notify_waiters();
    }
}

impl AdmissionFaultSink {
    async fn await_first(&self) -> ProcessWorkerFault {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = self.changed.notified();
                if let Some(fault) = self.faults.lock_recover().first().cloned() {
                    return fault;
                }
                changed.await;
            }
        })
        .await
        .expect("native process admission fault reached the sink")
    }
}

struct ProcessAdmissionScenario {
    raw_registry: Arc<TestLocalProcessRegistry>,
    registry: Arc<dyn ProcessRegistry>,
    process_work: NativeProcessWork,
    engine_started: Arc<tokio::sync::Notify>,
    engine_release: Arc<tokio::sync::Semaphore>,
    engine_runs: Arc<std::sync::Mutex<Vec<String>>>,
    fault_sink: AdmissionFaultSink,
    peer_lease: ProcessLease,
    admitted: Vec<String>,
}

impl ProcessAdmissionScenario {
    async fn new() -> Self {
        let raw_registry = Arc::new(TestLocalProcessRegistry::default());
        let registry: Arc<dyn ProcessRegistry> = raw_registry.clone();
        let watched = watch_process_registry(registry);
        let registry = Arc::clone(watched.registry());
        let engine_started = Arc::new(tokio::sync::Notify::new());
        let engine_release = Arc::new(tokio::sync::Semaphore::new(0));
        let engine_runs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = Arc::new(HeldSuccessEngine {
            started: Arc::clone(&engine_started),
            release: Arc::clone(&engine_release),
            runs: Arc::clone(&engine_runs),
        });
        let fault_sink = AdmissionFaultSink::default();

        let mut runtime_host = RuntimeHostConfig::in_memory(
            CommitBudget::bounded(1024 * 1024, 512),
            QueuedWorkBatchingConfig::new(1),
        );
        runtime_host.process_engines = ProcessEngineRegistry::new().with_engine(engine);
        let session_policy = SessionPolicy {
            provider_id: "sim-native-process-admission".to_string(),
            model: lash_core::ModelSpec::builder("sim-native-process-admission-model")
                .context_window_tokens(16_384)
                .build()
                .expect("valid admission scenario model"),
            ..SessionPolicy::new(TurnBudget::Unbounded)
        };
        let env_ref = lash_core::runtime::persist_process_execution_env(
            runtime_host.durability.process_env_store.as_ref(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::PluginOptions::default(),
                session_policy.clone(),
            ),
        )
        .await
        .expect("persist native process admission environment");

        for process_id in [HELD_PROCESS_ID, TERMINAL_PROCESS_ID, LEASED_PROCESS_ID] {
            registry
                .register_process(
                    ProcessRegistration::new(
                        process_id,
                        ProcessInput::Engine {
                            kind: "sim-native-process-admission".to_string(),
                            payload: json!({"scenario": "native_process_admission"}),
                        },
                        RecoveryContract::Rerunnable,
                        lash_core::ProcessProvenance::host(),
                    )
                    .with_execution_env_ref(Some(env_ref.clone())),
                )
                .await
                .expect("register native process admission row");
        }

        let peer_owner = LeaseOwnerIdentity::opaque(
            "native-process-admission-peer",
            "native-process-admission-peer:001",
        );
        let peer_lease = match registry
            .claim_process_lease(LEASED_PROCESS_ID, &peer_owner, PROCESS_LEASE_TTL_MS)
            .await
            .expect("claim peer-held process lease")
        {
            ProcessLeaseClaimOutcome::Acquired(lease) => lease,
            ProcessLeaseClaimOutcome::Busy { holder } => {
                panic!("fresh peer-held process lease was busy: {holder:?}")
            }
        };

        let mut worker_config = DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(vec![Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            )])),
            runtime_host,
            Arc::new(InMemorySessionStoreFactory::new()),
            WorkerProcessWork::SelfNative(watched.clone()),
            Arc::new(NoQueuedWork::new()),
            LeaseOwnerIdentity::opaque(
                "native-process-admission-worker",
                "native-process-admission-worker:001",
            ),
        )
        .with_session_policy(session_policy)
        .with_process_event_sink(Arc::new(fault_sink.clone()))
        .with_process_execution_concurrency(1)
        .expect("one native process execution slot");
        worker_config.native_substrate = non_default_native_substrate();
        let worker = DurableProcessWorker::new(worker_config)
            .expect("non-default native substrate validates at worker construction");
        let process_work = NativeProcessWork::new(&watched, worker);

        Self {
            raw_registry,
            registry,
            process_work,
            engine_started,
            engine_release,
            engine_runs,
            fault_sink,
            peer_lease,
            admitted: Vec::new(),
        }
    }

    async fn deliver(&mut self, event: &BoundaryEvent) -> Value {
        match event.label.as_str() {
            "native-process-admission.admit" => self.admit().await,
            "native-process-admission.peer-terminal" => self.peer_terminal().await,
            "native-process-admission.inject-fault" => self.inject_fault().await,
            "native-process-admission.observe" => self.observe().await,
            unexpected => panic!("unknown native process admission boundary `{unexpected}`"),
        }
    }

    async fn admit(&mut self) -> Value {
        let report = ProcessWorkSubstrate::admit_pending_processes(
            &self.process_work,
            "lash-sim native process admission",
        )
        .await
        .expect("native process work admits the first page");
        assert_eq!(report.intake, ProcessAdmissionIntake::Scanned);
        assert!(report.deferred.is_empty());
        assert_eq!(
            report.admitted,
            vec![HELD_PROCESS_ID.to_string()],
            "one native execution slot bounds the call's own intake; continuation rows remain in the scheduler-driven worklist"
        );
        self.admitted = report.admitted.clone();
        tokio::time::timeout(Duration::from_secs(5), self.engine_started.notified())
            .await
            .expect("held process entered its engine");
        json!({
            "outcome": "admitted",
            "intake": "scanned",
            "admitted": report.admitted,
            "deferred": 0,
            "worker_intake_page": 3,
        })
    }

    async fn peer_terminal(&mut self) -> Value {
        let peer_owner = LeaseOwnerIdentity::opaque(
            "native-process-admission-terminal-peer",
            "native-process-admission-terminal-peer:001",
        );
        let lease = match self
            .registry
            .claim_process_lease(TERMINAL_PROCESS_ID, &peer_owner, PROCESS_LEASE_TTL_MS)
            .await
            .expect("claim queued row before native worker reaches it")
        {
            ProcessLeaseClaimOutcome::Acquired(lease) => lease,
            ProcessLeaseClaimOutcome::Busy { holder } => {
                panic!("queued terminal-race row was already leased: {holder:?}")
            }
        };
        self.registry
            .complete_process_with_lease(
                &lease,
                ProcessAwaitOutput::from_tool_output(ToolCallOutput::success(json!({
                    "writer": "peer",
                    "terminal_before_native_claim": true,
                }))),
            )
            .await
            .expect("peer terminalizes queued admitted row");
        let terminal = self
            .process_work
            .await_terminal(TERMINAL_PROCESS_ID)
            .await
            .expect("native process awaiter observes the peer terminal");
        json!({
            "outcome": "terminal_by_peer",
            "process_id": TERMINAL_PROCESS_ID,
            "terminal": format!("{terminal:?}"),
        })
    }

    async fn inject_fault(&mut self) -> Value {
        self.raw_registry
            .set_process_terminal_write_error(Some(lash_core::PluginError::Session(
                "injected native process admission terminal-write failure".to_string(),
            )))
            .await;
        self.engine_release.add_permits(1);
        let fault = self.fault_sink.await_first().await;
        let ProcessWorkerFault::RecoveryBackendError {
            process_id,
            operation,
            error,
        } = fault
        else {
            panic!("expected a recovery backend fault from the admitted row")
        };
        assert_eq!(process_id, HELD_PROCESS_ID);
        assert_eq!(operation, ProcessRecoveryOperation::WriteTerminal);
        assert!(error.contains("injected native process admission"));
        json!({
            "outcome": "admitted_then_backend_fault",
            "process_id": process_id,
            "operation": operation.label(),
            "error_injected": true,
        })
    }

    async fn observe(&self) -> Value {
        let held = self
            .registry
            .get_process(HELD_PROCESS_ID)
            .await
            .expect("read faulted admitted row")
            .expect("faulted admitted row retained");
        let terminal = self
            .registry
            .get_process(TERMINAL_PROCESS_ID)
            .await
            .expect("read terminal-by-peer row")
            .expect("terminal-by-peer row retained");
        let leased = self
            .registry
            .get_process(LEASED_PROCESS_ID)
            .await
            .expect("read peer-leased row")
            .expect("peer-leased row retained");
        let current_peer_lease = self
            .registry
            .get_process_lease(LEASED_PROCESS_ID)
            .await
            .expect("read peer-held lease")
            .expect("peer-held lease remains live");
        let engine_runs = self.engine_runs.lock_recover().clone();

        assert_eq!(self.admitted, vec![HELD_PROCESS_ID.to_string()]);
        assert!(!held.status.is_terminal());
        assert_eq!(terminal.status, ProcessStatus::Completed);
        assert!(!leased.status.is_terminal());
        assert!(
            current_peer_lease
                .owner
                .same_incarnation(&self.peer_lease.owner)
        );
        assert_eq!(engine_runs, vec![HELD_PROCESS_ID.to_string()]);

        json!({
            "admitted_count": self.admitted.len(),
            "admitted_then_faulted": HELD_PROCESS_ID,
            "terminal_before_native_claim": TERMINAL_PROCESS_ID,
            "already_leased_by_peer": LEASED_PROCESS_ID,
            "native_engine_runs": engine_runs,
            "fault_count": self.fault_sink.faults.lock_recover().len(),
            "terminal_status": terminal.status.label(),
            "peer_lease_preserved": true,
        })
    }
}

async fn run_process_admission_scenario(seed: u64) -> Vec<DeliveredBoundary> {
    let events = [
        (1, "admit"),
        (2, "peer-terminal"),
        (3, "inject-fault"),
        (4, "observe"),
    ]
    .into_iter()
    .map(|(at, phase)| {
        BoundaryEvent::new(
            format!("native-process-admission:{phase}"),
            "native-process-admission",
            BoundaryKind::Worker,
            at,
            format!("native-process-admission.{phase}"),
            json!({"phase": phase}),
        )
    });
    let mut scheduler = BoundaryScheduler::with_events(seed, events);
    let mut scenario = ProcessAdmissionScenario::new().await;
    let mut delivered = Vec::new();
    while let Some(mut boundary) = scheduler.deliver_next(Value::Null) {
        boundary.observed = scenario.deliver(&boundary.as_event()).await;
        delivered.push(boundary);
    }
    delivered
}

#[test]
fn native_substrate_policy_scenario_exercises_non_default_pacing_and_millisecond_floor() {
    let first = run_pacing_scenario(0x434f_5633);
    let second = run_pacing_scenario(0x434f_5634);

    assert_eq!(first.len(), 5);
    assert_eq!(second.len(), 5);
    assert!(
        first
            .iter()
            .all(|event| event.scheduler.scheduler_controlled)
    );
    assert_eq!(first[0].scheduler.candidate_count_at_tick, 5);
    assert_ne!(
        first
            .iter()
            .map(|event| &event.boundary_id)
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|event| &event.boundary_id)
            .collect::<Vec<_>>(),
        "different seeds should interleave equal-tick pacing cases differently"
    );
    let case = |name: &str| {
        first
            .iter()
            .find(|event| event.observed["case"] == name)
            .unwrap_or_else(|| panic!("missing pacing case `{name}`"))
    };
    assert_eq!(case("valid-non-default").observed["valid"], true);
    assert_eq!(case("slow-wake-one-millisecond").observed["valid"], true);
    assert_eq!(case("slow-wake-sub-millisecond").observed["valid"], false);
    assert_eq!(
        case("slow-wake-sub-millisecond").observed["slow_wake_threshold_micros"],
        999
    );
}

#[tokio::test]
async fn native_process_admission_scenario_interleaves_peer_lease_terminal_race_and_fault() {
    let delivered = run_process_admission_scenario(0x434f_5633).await;

    assert_eq!(delivered.len(), 4);
    assert!(
        delivered
            .iter()
            .all(|event| event.scheduler.scheduler_controlled)
    );
    assert_eq!(delivered[0].observed["outcome"], "admitted");
    assert_eq!(delivered[1].observed["outcome"], "terminal_by_peer");
    assert_eq!(
        delivered[2].observed["outcome"],
        "admitted_then_backend_fault"
    );
    assert_eq!(delivered[3].observed["admitted_count"], 1);
    assert_eq!(delivered[3].observed["peer_lease_preserved"], true);
    assert_eq!(delivered[3].observed["fault_count"], 1);
}
