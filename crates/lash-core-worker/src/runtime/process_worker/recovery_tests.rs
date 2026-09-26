use crate::{ProcessEventLog as _, ProcessQuery as _, ProcessRegistrar as _};
use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::test_backend::*;
use super::*;
use crate::TestProcessRegistryWriteExt;
use crate::{
    AbandonRequest, LeaseOwnerIdentity, ProcessExecutionEnvRef, ProcessInput, ProcessListFilter,
    ProcessRegistration, ProcessStarted, ProcessStatus, TriggerStore,
};
use lash_core::testing::trace_capture::{CapturedFieldKind, EventCapture, capturing};

mod attachment_owner_tests;
mod drain_report_tests;
mod fault_surface_tests;
mod generation_fence_tests;
mod pagination_tests;
mod parent_end_redrive_tests;
#[path = "recovery_disposition_tests.rs"]
mod recovery_disposition_tests;
mod session_store_factories;
mod session_turn_cancellation_tests;
mod session_turn_refusal_tests;
mod worker_fixtures;
use session_store_factories::*;
use worker_fixtures::*;

const TEST_PROCESS_EXECUTION_CONCURRENCY: usize = 4;

fn test_session_policy() -> crate::SessionPolicy {
    crate::SessionPolicy {
        provider_id: "test".to_string(),
        model: crate::ModelSpec::builder("test-model")
            .context_window_tokens(16_384)
            .build()
            .expect("valid model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    }
}

fn session_turn_registration(child_session_id: &SessionId) -> ProcessRegistration {
    let create_request = crate::SessionCreateRequest::child_session(
        "recovery-test-parent",
        crate::SessionStartPoint::Empty,
        crate::PluginOptions::default(),
    )
    .with_session_id(child_session_id);
    ProcessRegistration::new(
        ProcessInput::SessionTurn {
            definition_key: "recovery-test-session-turn:v1".to_string(),
            create_request: Box::new(create_request),
            turn_input: Box::new(crate::TurnInput::text("run recovered child turn")),
            output_contract: crate::ToolOutputContract::Static,
        },
        RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

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
async fn crash_replay_observes_durable_cancellation_before_rerunning_process() {
    let backend = memory_backend().await;
    let raw_registry = backend.process_registry();
    let watched = crate::watch_process_registry(raw_registry);
    let registry = Arc::clone(watched.registry());
    let _process_id = "cancelled-before-crash-replay";
    let cancelled_before_crash_replay_record = registry
        .register_process(session_turn_registration(&SessionId::from(
            "cancelled-before-crash-child",
        )))
        .await
        .expect("register replay fixture");
    let process_id = cancelled_before_crash_replay_record.id.clone();
    registry
        .append_event(
            &process_id,
            crate::ProcessEventAppendRequest::cancel_requested(&registry.require_process_id(&process_id).await.expect("retained cancellation target"),
&crate::CancelRequest::new(crate::CancelOrigin::OperatorRequested, "actor:fixture:crash_replay_observes_durable_cancellation_before_rerunning_process", 11)),
        )
        .await
        .expect("persist cancellation before simulated crash");
    let policy = test_session_policy();
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            test_host_config(&backend),
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("cancel-replay-worker", "host-a", "fresh-incarnation"),
        )
        .with_session_policy(policy),
    )
    .expect("valid cancel-replay worker");

    let report = worker
        .drive_pending_processes()
        .await
        .expect("fresh worker admits cancelled replay");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    await_terminal(&registry, &process_id).await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read replayed process")
        .expect("replayed process remains retained");
    assert_eq!(
        record.status,
        ProcessStatus::Cancelled,
        "a fresh recovery attempt must settle the persisted cancellation before recreating the child"
    );
}

#[tokio::test]
async fn committed_session_turn_cancellation_fences_a_successful_runner_terminal() {
    let provider_started = Arc::new(tokio::sync::Notify::new());
    let provider_release = Arc::new(tokio::sync::Semaphore::new(0));
    let started = Arc::clone(&provider_started);
    let release = Arc::clone(&provider_release);
    let provider = crate::testing::TestProvider::builder()
        .kind("test")
        .complete(move |_request| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                started.notify_one();
                let _permit = release.acquire().await.expect("provider release permit");
                Ok(crate::llm::types::LlmResponse {
                    parts: vec![crate::llm::types::LlmOutputPart::Text {
                        text: "runner completed".to_string(),
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let (backend, raw_registry) = faulted_memory_backend().await;
    let raw_registry_port = backend.process_registry();
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (registry, _hub, process_work) =
        late_bound_process_work_wiring(raw_registry_port, Arc::clone(&run_handle));
    let mut runtime_host = test_host_config(&backend);
    runtime_host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider));
    let policy = test_session_policy();
    let watcher_ready = Arc::new(tokio::sync::Notify::new());
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        runtime_host,
        crate::WorkerProcessWork::External(process_work),
        Arc::new(crate::NoSessionWork::new()),
        local_owner("terminal-fence-worker", "host-a", "terminal-fence-start"),
    )
    .with_session_policy(policy);
    config.cancel_watcher_ready = Some(Arc::clone(&watcher_ready));
    let worker = DurableProcessWorker::new(config).expect("valid terminal-fence worker");
    run_handle
        .worker
        .set(worker)
        .unwrap_or_else(|_| panic!("terminal-fence worker binds once"));
    let _process_id = "session-turn-terminal-fence";
    let session_turn_terminal_fence_record = registry
        .register_process(session_turn_registration(&SessionId::from(
            "session-turn-terminal-fence-child",
        )))
        .await
        .expect("register SessionTurn fence fixture");
    let process_id = session_turn_terminal_fence_record.id.clone();

    let report = run_handle
        .enable_and_drive()
        .await
        .expect("admit SessionTurn fence fixture");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    provider_started.notified().await;
    tokio::time::timeout(Duration::from_secs(5), watcher_ready.notified())
        .await
        .expect("cancel watcher reaches its durable wait before runner completion");
    // The ready hook explicitly polled the watcher to Pending. Check the durable
    // precondition separately: cancellation has not yet been committed.
    assert!(
        raw_registry
            .get_process(&process_id)
            .await
            .expect("read before racing cancellation")
            .expect("retained running target")
            .cancel_request
            .is_none()
    );
    raw_registry
        .append_event(
            &process_id,
            crate::ProcessEventAppendRequest::cancel_requested(&raw_registry.require_process_id(&process_id).await.expect("retained cancellation target"),
&crate::CancelRequest::new(crate::CancelOrigin::OperatorRequested, "actor:fixture:committed_session_turn_cancellation_fences_a_successful_runner_terminal", 11)),
        )
        .await
        .expect("append cancellation without notifying the parked watcher");
    // Release the runner in the same step that follows the durable
    // cancellation commit. The store runs on its own thread, so a paused clock
    // would jump across every store wait rather than freeze the watcher's
    // poll; the runner and the watcher race on real time, and whichever wins,
    // the committed cancellation is the recorded terminal.
    provider_release.add_permits(1);

    // The runner settles its child turn and returns a successful output, but
    // the committed cancellation outranks it: the recorded terminal is
    // `Cancelled`, and the child session stays retained.
    await_terminal(&registry, &process_id).await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read fenced SessionTurn")
        .expect("fenced SessionTurn remains retained");
    assert_eq!(
        record.status,
        ProcessStatus::Cancelled,
        "a committed SessionTurn cancellation must not write the runner's successful terminal"
    );
}

#[tokio::test]
async fn dispatcher_unwind_clears_running_latch_and_notifies() {
    let scheduler = Arc::new(ProcessExecutionScheduler::new(
        ProcessExecutionConcurrency::new(1).expect("valid test concurrency"),
        None,
    ));
    let continuation = crate::ProcessWorklistCursor::new(
        "test",
        crate::ProcessId::fixture("after-panic-boundary"),
        crate::ProcessId::fixture("through-panic-boundary"),
    );
    {
        let mut state = scheduler.state.lock_recover();
        assert!(state.claim_dispatcher());
        state.extra = ProcessWorklistScan::Fetching {
            continuation: Some(continuation.clone()),
            rescan: false,
        };
    }
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
        !scheduler.state.lock_recover().dispatcher_running(),
        "a later drive pass must be able to start a replacement dispatcher"
    );
    let state = scheduler.state.lock_recover();
    assert!(
        matches!(
            &state.extra,
            ProcessWorklistScan::Ready {
                continuation: Some(restored),
                ..
            } if restored == &continuation
        ),
        "a later dispatcher must retry the cursor whose fetch panicked"
    );
}

#[derive(Default)]
struct LateBoundProcessWork {
    worker: OnceLock<DurableProcessWorker>,
    enabled: AtomicBool,
}

impl LateBoundProcessWork {
    async fn enable_and_drive(&self) -> Result<ProcessAdmissionReport, PluginError> {
        self.enabled.store(true, Ordering::SeqCst);
        self.worker
            .get()
            .expect("test process worker is bound before execution")
            .drive_pending_processes()
            .await
    }
}

#[async_trait::async_trait]
impl crate::ProcessWorkSubstrate for LateBoundProcessWork {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Ok(ProcessAdmissionReport::default());
        }
        self.worker
            .get()
            .expect("test process worker is bound before execution")
            .drive_pending_processes()
            .await
    }

    async fn await_process_terminal(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<crate::ProcessTerminalWait, PluginError> {
        crate::NativeProcessWork::for_registry(Arc::clone(
            self.worker
                .get()
                .expect("test process worker is bound before execution")
                .config
                .process_registry(),
        ))
        .await_terminal(process_id)
        .await
        .map(crate::ProcessTerminalWait::Terminal)
    }
}

fn late_bound_process_work_wiring(
    registry: Arc<dyn ProcessRegistry>,
    process_work: Arc<LateBoundProcessWork>,
) -> (
    Arc<dyn ProcessRegistry>,
    crate::ProcessChangeHub,
    crate::ProcessWorkWiring,
) {
    let watched = crate::watch_process_registry(registry);
    let registry = Arc::clone(watched.registry());
    let hub = watched.hub().clone();
    let port: Arc<dyn crate::ProcessWorkSubstrate> = process_work;
    let wiring = crate::ProcessWorkWiring::new(watched, port);
    (registry, hub, wiring)
}

/// End-to-end shape of the re-entrancy F1 came from: the reconcile registers a
/// process and drives it through the work driver, then the outer pass's own
/// scan sees that row already scheduled. The row belongs to this one call, so
/// it must appear once as admitted and never as another owner's contention.
#[tokio::test]
async fn a_reentrant_reconcile_drive_reports_its_row_once_as_admitted() {
    let backend = memory_backend().await;
    let trigger_store = backend.trigger_store();
    let delivery = seed_reserved_trigger_delivery(&trigger_store).await;
    let run_handle = Arc::new(LateBoundProcessWork::default());
    run_handle.enabled.store(true, Ordering::SeqCst);
    let worker = reentrant_worker(
        &backend,
        local_owner("reentrant-worker", "host-a", "claimant-start"),
        Arc::clone(&run_handle),
    )
    .await;

    let report = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    let process_id = bound_delivery_process(&trigger_store, &delivery).await;

    assert_eq!(
        report
            .admitted
            .iter()
            .filter(|id| id.as_str() == process_id.as_str())
            .count(),
        1,
        "the reconciled row is this call's admission, exactly once: {report:?}"
    );
    assert!(
        !report
            .deferred
            .iter()
            .any(|entry| entry.process_id == process_id),
        "a call must never report its own admission as a deferral: {report:?}"
    );
    assert_eq!(
        report.intake,
        ProcessAdmissionIntake::Scanned,
        "the outer pass read the worklist itself"
    );
}

/// A registration with an explicit disposition; the disposition-driven sweep keys off the
/// declared disposition, not the input kind, so these unit tests exercise the
/// verdict without standing up execution infrastructure.
fn registration_with_disposition(disposition: crate::RecoveryContract) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        disposition,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
}

async fn abandoned_evidence(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> crate::AbandonEvidence {
    let record = registry
        .get_process(process_id)
        .await
        .expect("read process")
        .expect("process exists");
    match (record.status, record.outcome) {
        (ProcessStatus::Abandoned, Some(ProcessAwaitOutput::Abandoned { evidence, .. })) => {
            *evidence
        }
        other => panic!("expected an Abandoned terminal, got {other:?}"),
    }
}

fn assert_recovery_backend_error_event(
    capture: &EventCapture,
    process_id: &ProcessId,
    operation: &str,
    error: &str,
) {
    let event = capture.exactly_one("process_recovery.backend_error");
    assert_eq!(event.level, "WARN");
    assert_eq!(event.target, "lash_core::process_recovery");
    let expected = [
        (
            "event",
            "process_recovery.backend_error",
            CapturedFieldKind::Str,
        ),
        ("decision_basis", "backend_error", CapturedFieldKind::Str),
        ("process_id", process_id, CapturedFieldKind::Str),
        ("operation", operation, CapturedFieldKind::Str),
        ("outcome", "deferred", CapturedFieldKind::Str),
        ("error", error, CapturedFieldKind::Str),
        (
            "message",
            "process recovery backend operation failed; row deferred",
            CapturedFieldKind::Debug,
        ),
    ];
    assert_eq!(event.field_count(), expected.len());
    for (field, value, kind) in expected {
        assert_eq!(event.field_kind(field), kind, "field kind for {field}");
        assert_eq!(event.field(field), value, "field value for {field}");
    }
}

fn local_owner(owner_id: &str, _host_id: &str, _process_start: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
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
    let process_env_ref = crate::testing::process_execution_env_fixture_ref();
    crate::TriggerSubscriptionDraft::for_process(
        "recovery-test",
        process_env_ref,
        "ui.button.pressed",
        source_key,
        ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: serde_json::json!({ "target": "reconcile" }),
        },
        crate::ProcessIdentity::new("testing-fixture"),
    )
    .with_payload_schema(crate::LashSchema::any())
}

/// The process a reserved delivery's start bound to it (ADR 0107).
async fn bound_delivery_process(
    trigger_store: &Arc<dyn TriggerStore>,
    delivery: &crate::TriggerDeliveryReservation,
) -> ProcessId {
    trigger_store
        .list_deliveries_by_occurrence_id(&delivery.occurrence.occurrence_id)
        .await
        .expect("list the occurrence's deliveries")
        .into_iter()
        .find(|reserved| {
            reserved.subscription.subscription_id == delivery.subscription.subscription_id
        })
        .and_then(|reserved| reserved.process_id)
        .expect("the delivery's start bound its process")
}

/// Every retained process, whatever its id: a start key that started twice
/// would show here as a second row.
async fn all_process_count(registry: &Arc<dyn ProcessRegistry>) -> usize {
    registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list processes")
        .len()
}

async fn await_terminal(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
    let awaiter = crate::NativeProcessWork::for_registry(Arc::clone(registry));
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
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process_id": context.process_id()}),
            ))
            .into(),
        )
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
                    program_hash: "program-v1".to_string(),
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
            Ok(crate::ProcessRunOutcome::Terminal {
                output: Box::new(ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::json!({ "segments": 2 })),
                )),
                prelude: Vec::new(),
            })
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
    run_handle: Arc<LateBoundProcessWork>,
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
        self.payloads.lock_recover().push(payload);
        Ok(crate::ProcessRunOutcome::Terminal {
            output: Box::new(ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::success(serde_json::json!({ "recorded": true })),
            )),
            prelude: Vec::new(),
        })
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
        Ok(crate::ProcessRunOutcome::Terminal {
            output: Box::new(ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::success(serde_json::json!({ "nested": "done" })),
            )),
            prelude: Vec::new(),
        })
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

    /// Starting a process and awaiting it inside the tool call is orchestration,
    /// not a recorded leaf attempt, so this registers in the orchestrating lane.
    #[expect(
        unsafe_code,
        reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
    )]
    fn orchestrating() -> crate::tool_provider::orchestration::OrchestratingToolDef {
        let implementation: Arc<
            dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
        > = Arc::new(Self);
        // SAFETY: lash-core owns this test-only tool contract and its body.
        unsafe {
            crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(
                implementation,
            )
        }
    }
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation
    for NestedProcessWaitTool
{
    fn manifest(&self) -> crate::ToolManifest {
        Self::definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(Self::definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        assert!(
            PROCESS_EXECUTION_PERMIT.try_with(|_| ()).is_ok(),
            "production child turn must inherit the outer process execution permit"
        );
        let start_key = match context.start_key(0) {
            Ok(start_key) => start_key,
            Err(err) => {
                return crate::ToolOutcome::err_fmt(format_args!(
                    "nested process start has no key: {err}"
                ));
            }
        };
        let request = crate::ProcessStartRequest::new(
            ProcessInput::Engine {
                kind: "nested-process-test".to_string(),
                payload: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            crate::ProcessOriginator::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
        .with_start_key(Some(start_key));
        let process_id = match context.start_process(request).await {
            Ok(started) => started.process_id,
            Err(err) => {
                return crate::ToolOutcome::err_fmt(format_args!(
                    "failed to start nested process: {err}"
                ));
            }
        };
        match context.await_process(&process_id).await {
            Ok(ProcessAwaitOutput::Settled { output }) if output.is_success() => {
                crate::ToolOutcome::ok(serde_json::json!({ "nested": "done" }))
            }
            Ok(other) => crate::ToolOutcome::err_fmt(format_args!(
                "nested process returned non-success output: {other:?}"
            )),
            Err(err) => {
                crate::ToolOutcome::err_fmt(format_args!("failed to await nested process: {err}"))
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

    fn success(process_id: ProcessId) -> crate::ProcessRunOutcome {
        crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
            serde_json::json!({ "completed": process_id }),
        ))
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
        let process_id = context.process_id().clone();
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
                let registration = crate::ProcessStartRequest::new(
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
                    RecoveryContract::Rerunnable,
                    runtime.trigger_actor(),
                    crate::ProcessLifecyclePolicy::new(
                        runtime
                            .child_process_parent_scope()
                            .expect("runtime parent"),
                        crate::OnParentEnd::Abandon,
                    ),
                )
                .with_start_key(Some(crate::StartKey::for_host(
                    crate::StartKeyOwner::HOST,
                    format!("chain-root-{root:03}"),
                )));
                let reply = runtime
                    .start_child_process(registration, "test-chain", None)
                    .await;
                assert!(
                    reply.output.is_success(),
                    "production root start failed: {:?}",
                    reply.output
                );
            }
            let _ = self
                .state
                .run_handle
                .enable_and_drive()
                .await
                .expect("drive production roots");
            drop(runtime);
            runtime_guard
                .shutdown(false)
                .await
                .expect("finish launcher runtime context");
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
            let child_key = format!("{}-node-{root:03}-{child_level:02}", 20 + child_level);
            let registration = crate::ProcessStartRequest::new(
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
                RecoveryContract::Rerunnable,
                runtime.trigger_actor(),
                crate::ProcessLifecyclePolicy::new(
                    runtime
                        .child_process_parent_scope()
                        .expect("runtime parent"),
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_start_key(Some(crate::StartKey::for_host(
                crate::StartKeyOwner::HOST,
                &child_key,
            )));
            let reply = runtime
                .start_child_process(registration, "test-chain", None)
                .await;
            assert!(
                reply.output.is_success(),
                "production child start failed: {:?}",
                reply.output
            );
            let child_id = crate::process_id_from_handle_json(&reply.output.value_for_projection())
                .expect("the start answers the child's handle");
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
        runtime_guard
            .shutdown(false)
            .await
            .expect("finish process runtime context");
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
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
    let p_00_chain_launcher_record = registry
        .register_process(engine_registration(
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
        let _ = worker
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
        "native process execution exceeded its configured concurrency"
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
        .filter(|record| record.id != p_00_chain_launcher_record.id)
    {
        assert_eq!(record.lifecycle.on_parent_end, crate::OnParentEnd::Abandon);
        let crate::ParentScope::Owned(crate::EffectOpener::Process { process_id }) =
            &record.lifecycle.parent
        else {
            panic!(
                "a process-started child must retain its process parent: {}",
                record.id
            );
        };
        records
            .iter()
            .find(|candidate| candidate.id == process_id)
            .expect("parent is retained in the chain");
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
async fn process_session_turn_wait_releases_outer_run_permit() {
    // Process session turns cross a fresh Tokio task stack through the same
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let backend = memory_backend().await;
    let raw_registry = backend.process_registry();
    let (registry, _hub, process_work) =
        late_bound_process_work_wiring(raw_registry, Arc::clone(&run_handle));
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(nested_engine));
    runtime_host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider));
    let policy = test_session_policy();
    let mut plugin_factories = crate::testing::test_standard_protocol_factories();
    plugin_factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        "nested-process-wait-tool",
        crate::PluginSpec::new().with_orchestrating_tool(NestedProcessWaitTool::orchestrating()),
    )));
    let worker = DurableProcessWorker::new({
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(plugin_factories)),
            runtime_host,
            crate::WorkerProcessWork::External(process_work),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("session-turn-worker", "host-a", "session-turn-start"),
        )
        .with_session_policy(policy.clone())
        .with_process_execution_concurrency(1)
        .expect("valid test process execution concurrency")
    })
    .expect("valid test native substrate config");
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
    )
    .with_session_id("nested-wait-child");
    let outer_session_turn_record = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::SessionTurn {
                definition_key: "nested-session-turn:v1".to_string(),
                create_request: Box::new(child_request),
                turn_input: Box::new(crate::TurnInput::text("await nested process")),
                output_contract: crate::ToolOutputContract::Static,
            },
            RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register production session-turn process");
    let outer_process_id = outer_session_turn_record.id.clone();

    tokio::time::timeout(Duration::from_secs(10), async {
        let _ = run_handle
            .enable_and_drive()
            .await
            .expect("drive production session-turn process");
        wait_for_terminal_count(&registry, 2, "session-turn process and its nested process").await;
    })
    .await
    .expect("production SessionTurn path completes without permit starvation");
    let outer = crate::NativeProcessWork::for_registry(Arc::clone(&registry))
        .await_terminal(&outer_process_id)
        .await
        .expect("outer session-turn process is terminal");
    assert!(
        matches!(
            outer,
            ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ),
        "outer session-turn process must succeed: {outer:?}"
    );
    assert_eq!(nested_runs.load(Ordering::SeqCst), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn segment_boundary_reenters_in_memory_without_premature_terminal() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(BoundaryThenTerminalEngine {
            runs: Arc::clone(&runs),
        })),
    );
    let policy = test_session_policy();
    let env_spec =
        crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone());
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("boundary-recovery-test"),
        &env_spec,
    )
    .await
    .expect("persist process env");
    let worker = DurableProcessWorker::new({
        let watched =
            crate::watch_process_registry(Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>);
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            runtime_host,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner("segment-worker", "host-a", "start-a"),
        )
        .with_session_policy(policy)
    })
    .expect("valid test native substrate config");
    let registered = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::Engine {
                    kind: "boundary-test".to_string(),
                    payload: serde_json::json!({}),
                },
                RecoveryContract::Rerunnable,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .expect("register process");

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive process");
    await_terminal(&registry, &registered.id).await;
    let final_record = registry
        .get_process(&registered.id)
        .await
        .expect("read process")
        .expect("process exists");
    assert_eq!(runs.load(Ordering::SeqCst), 2, "{:?}", final_record.status);
    assert!(matches!(final_record.status, ProcessStatus::Completed));
}

#[tokio::test]
async fn sweep_reconciles_reserved_trigger_delivery_without_process() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let trigger_store = backend.trigger_store();
    let delivery = seed_reserved_trigger_delivery(&trigger_store).await;
    assert_eq!(
        delivery.process_id, None,
        "test starts in the reserve/start crash window: the reservation is unbound"
    );
    assert_eq!(all_process_count(&registry).await, 0);

    let worker = native_worker(
        &backend,
        local_owner("trigger-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");

    let process_id = bound_delivery_process(&trigger_store, &delivery).await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("sweep registers missing trigger delivery process");
    assert_eq!(all_process_count(&registry).await, 1);
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

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("second sweep dispatches");
    assert_eq!(
        all_process_count(&registry).await,
        1,
        "re-running the sweep must not create a duplicate process row"
    );
    assert_eq!(
        bound_delivery_process(&trigger_store, &delivery).await,
        process_id,
        "the reservation stays bound to the process its start key registered"
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
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let trigger_store = backend.trigger_store();
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(SnapshotRecordingEngine {
            payloads: Arc::clone(&payloads),
        })),
    );
    let policy = test_session_policy();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("snapshot-recovery-test"),
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
    let worker = DurableProcessWorker::new({
        let watched =
            crate::watch_process_registry(Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>);
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            runtime_host,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            local_owner(
                "snapshot-recovery-worker",
                "host-a",
                "snapshot-recovery-start",
            ),
        )
        .with_session_policy(policy)
    })
    .expect("valid test native substrate config");
    (registry, trigger_store, delivery, payloads, worker)
}

#[tokio::test]
async fn sweep_recovers_reserved_v1_snapshot_after_v2_update_exactly_once() {
    let (registry, trigger_store, delivery, payloads, worker) =
        snapshot_recovery_fixture(false).await;

    let _ = worker.drive_pending_processes().await.expect("recover v1");
    let process_id = bound_delivery_process(&trigger_store, &delivery).await;
    await_terminal(&registry, &process_id).await;
    let terminal = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("recovered delivery process");
    assert!(
        matches!(terminal.status, ProcessStatus::Completed),
        "recovered delivery must complete: {:?}",
        terminal.status
    );
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("repeat recovery sweep");

    assert_eq!(delivery.subscription.revision, 1);
    assert_eq!(
        payloads.lock_recover().as_slice(),
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

    let _ = worker.drive_pending_processes().await.expect("recover v1");
    let process_id = bound_delivery_process(&trigger_store, &delivery).await;
    await_terminal(&registry, &process_id).await;
    let terminal = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("recovered delivery process");
    assert!(
        matches!(terminal.status, ProcessStatus::Completed),
        "recovered delivery must complete: {:?}",
        terminal.status
    );
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("repeat recovery sweep");

    assert_eq!(delivery.subscription.revision, 1);
    assert_eq!(
        payloads.lock_recover().as_slice(),
        [serde_json::json!({ "args": {}, "config": "v1" })]
    );
}

#[tokio::test]
async fn sweep_does_not_reconcile_trigger_delivery_pruned_with_terminal_process() {
    let backend = memory_backend().await;
    let trigger_store = backend.trigger_store();
    let registry = backend.process_registry();
    let trigger_store_dyn: Arc<dyn TriggerStore> = trigger_store.clone();
    let delivery = seed_reserved_trigger_delivery(&trigger_store_dyn).await;
    assert_eq!(
        delivery.process_id, None,
        "test starts in the reserve/start crash window"
    );

    let worker = native_worker(
        &backend,
        local_owner("trigger-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    let process_id = bound_delivery_process(&trigger_store_dyn, &delivery).await;
    registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("sweep registers missing trigger delivery process");

    let terminal = registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({ "done": true }),
            )),
            crate::ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("complete trigger delivery process");
    let report = registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune completed trigger delivery process");
    assert_eq!(report.pruned_processes, 1);
    crate::reconcile_pruned_trigger_deliveries(registry.as_ref(), trigger_store.as_ref(), None)
        .await
        .expect("reconcile pruned trigger deliveries");
    assert!(
        matches!(
            registry.get_process(&process_id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "terminal trigger delivery process is pruned"
    );
    assert!(
        trigger_store
            .list_deliveries_by_process_id(&process_id)
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
            if receipt.disposition == crate::TriggerMutationOutcome::Created
    ));

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("post-prune sweep dispatches");
    assert_eq!(
        all_process_count(&registry).await,
        0,
        "recovery sweep must not resurrect a delivery whose terminal process was pruned"
    );
}

#[tokio::test]
async fn sweep_does_not_reconcile_trigger_delivery_when_process_exists() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let trigger_store = backend.trigger_store();
    let delivery = seed_reserved_trigger_delivery(&trigger_store).await;
    // A process already registered under the delivery's start key: the start
    // that crashed before binding it (ADR 0107).
    let start_key = crate::StartKey::for_trigger_delivery(
        &delivery.occurrence.occurrence_id,
        &delivery.subscription.subscription_id,
        &delivery.subscription.incarnation,
        delivery.subscription.revision,
    );
    let existing = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::json!({ "already": "registered" }),
                },
                RecoveryContract::Rerunnable,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_start_key(Some(start_key)),
        )
        .await
        .expect("pre-register delivery process");

    let worker = native_worker(
        &backend,
        local_owner("trigger-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");

    assert_eq!(
        bound_delivery_process(&trigger_store, &delivery).await,
        existing.id,
        "recovery binds the process its start key already registered"
    );
    let record = registry
        .get_process(&existing.id)
        .await
        .expect("read process")
        .expect("existing process remains");
    assert_eq!(record.provenance.caused_by, None);
    assert_eq!(
        all_process_count(&registry).await,
        1,
        "existing process row must be treated as already started"
    );
}

/// ExternallyOwned rows are never claimed and never run: lash does not own
/// their execution (ADR 0019).
#[tokio::test]
async fn sweep_never_claims_externally_owned_rows() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_ext_record = registry
        .register_process(registration_with_disposition(
            RecoveryContract::ExternallyOwned,
        ))
        .await
        .expect("register");

    let worker = native_worker(
        &backend,
        local_owner("live-worker", "host-a", "claimant-start"),
    )
    .await;
    let report = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    // The row is not an admission on either tier: lash never executes it, so
    // one registry reads the same whichever tier drove it.
    assert!(report.admitted.is_empty());
    assert_eq!(
        report.deferred,
        vec![ProcessAdmissionDeferred {
            process_id: proc_ext_record.id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
        }]
    );
    // A second pass before the dispatcher drains the row must say the same
    // thing: an externally-owned row is never this worker's contention.
    let second = worker
        .drive_pending_processes()
        .await
        .expect("second sweep");
    assert!(second.admitted.is_empty());
    assert_eq!(
        second.deferred,
        vec![ProcessAdmissionDeferred {
            process_id: proc_ext_record.id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::ExternallyOwned,
        }]
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    let record = registry
        .get_process(&proc_ext_record.id)
        .await
        .expect("read process")
        .expect("process");
    assert!(
        !record.is_terminal(),
        "an externally-owned row must never be claimed or run by the sweep"
    );
    assert!(
        registry
            .get_process_lease(&proc_ext_record.id)
            .await
            .expect("lease read")
            .is_none(),
        "the sweep must not claim a lease on an externally-owned row"
    );
}

/// A rerunnable process whose declared attempt budget is already consumed is
/// terminalized by recovery without invoking the engine again.
#[tokio::test]
async fn sweep_terminalizes_exhausted_attempt_budget_as_engine_gave_up() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_attempts_exhausted_record = registry
        .register_process(
            registration_with_disposition(RecoveryContract::Rerunnable).with_max_attempts(Some(1)),
        )
        .await
        .expect("register attempt-exhausted process");
    let exhausted_owner = LeaseOwnerIdentity::opaque("exhausted-owner", "exhausted-incarnation");
    registry
        .record_first_started(
            &proc_attempts_exhausted_record.id,
            ProcessStarted {
                owner: exhausted_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record exhausted attempt");

    let worker = native_worker(
        &backend,
        local_owner("recovery-worker", "host-b", "recovery-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches exhausted process");
    await_terminal(&registry, &proc_attempts_exhausted_record.id).await;

    let evidence = abandoned_evidence(&registry, &proc_attempts_exhausted_record.id).await;
    assert_eq!(evidence.writer, AbandonWriter::EngineGaveUp);
    assert_eq!(
        evidence.owner,
        Some(exhausted_owner),
        "engine-gave-up evidence must retain the exhausted attempt owner"
    );
}

/// A pending Abandon Request on an externally-owned row is reconciled into
/// `Abandoned{reconciled_request}` — there is no owner lease to wait out.
#[tokio::test]
async fn sweep_reconciles_externally_owned_abandon_request() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_ext_abandon_record = registry
        .register_process(registration_with_disposition(
            RecoveryContract::ExternallyOwned,
        ))
        .await
        .expect("register");
    registry
        .request_process_abandon(
            &proc_ext_abandon_record.id,
            AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 1,
                reason: Some("host retired".to_string()),
            },
        )
        .await
        .expect("request abandon");

    let worker = native_worker(
        &backend,
        local_owner("live-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    await_terminal(&registry, &proc_ext_abandon_record.id).await;

    let evidence = abandoned_evidence(&registry, &proc_ext_abandon_record.id).await;
    assert_eq!(evidence.writer, AbandonWriter::ReconciledRequest);
    assert!(
        evidence.owner.is_none(),
        "externally-owned work names no lash execution owner"
    );
}

/// A started OwnerBound row with no Abandon Request is left non-terminal —
/// elapsed time alone never terminalizes.
#[tokio::test]
async fn sweep_skips_started_owner_bound_with_silent_holder() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_ob_silent_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register");
    registry
        .record_first_started(
            &proc_ob_silent_record.id,
            ProcessStarted {
                owner: LeaseOwnerIdentity::opaque("started-worker", "started-incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record started");
    // A live holder keeps the row unavailable until its TTL expires.
    registry
        .claim_process_lease(
            &proc_ob_silent_record.id,
            &LeaseOwnerIdentity::opaque("other-worker", "other-incarnation"),
            60_000,
        )
        .await
        .expect("live holder claims")
        .acquired()
        .expect("live holder lease acquired");

    let worker = native_worker(
        &backend,
        local_owner("live-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let record = registry
        .get_process(&proc_ob_silent_record.id)
        .await
        .expect("read process")
        .expect("process");
    assert!(
        !record.is_terminal(),
        "a holder with no abandon request stays non-terminal"
    );
}

/// A started OwnerBound row with a lapsed lease and a pending Abandon Request
/// is reconciled into `Abandoned{reconciled_request}`, naming the started
/// owner as the lapsed owner.
#[tokio::test]
async fn sweep_reconciles_started_owner_bound_after_lease_lapse() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_ob_lapse_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register");
    registry
        .record_first_started(
            &proc_ob_lapse_record.id,
            ProcessStarted {
                owner: LeaseOwnerIdentity::opaque("lapsed-owner", "lapsed-incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record started");
    registry
        .request_process_abandon(
            &proc_ob_lapse_record.id,
            AbandonRequest {
                requested_by: "operator".to_string(),
                requested_at_ms: 2,
                reason: None,
            },
        )
        .await
        .expect("request abandon");
    // No live lease held: the row's owner lease has lapsed.

    let worker = native_worker(
        &backend,
        local_owner("live-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    await_terminal(&registry, &proc_ob_lapse_record.id).await;

    let evidence = abandoned_evidence(&registry, &proc_ob_lapse_record.id).await;
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
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let proc_ob_unstarted_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register");

    let worker = native_worker(
        &backend,
        local_owner("live-worker", "host-a", "claimant-start"),
    )
    .await;
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("sweep dispatches");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry
                .get_process(&proc_ob_unstarted_record.id)
                .await
                .expect("read process")
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
                .claim_process_lease(&proc_ob_unstarted_record.id, &next_owner, 60_000)
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
        .get_process(&proc_ob_unstarted_record.id)
        .await
        .expect("read process")
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(SnapshotRecordingEngine {
            payloads: Arc::new(Mutex::new(Vec::new())),
        }),
        run_handle,
    )
    .await;
    let missing_engine_record = registry
        .register_process(engine_registration(
            "not-installed",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register missing engine row");
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("drive missing engine row");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let record = registry
                .get_process(&missing_engine_record.id)
                .await
                .expect("read process")
                .expect("missing engine row");
            let lease = registry
                .get_process_lease(&missing_engine_record.id)
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
            &missing_engine_record.id,
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
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (_worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(FailOnceArtifactReadEngine {
            reads: Arc::clone(&reads),
        }),
        run_handle,
    )
    .await;
    let artifact_read_retry_record = registry
        .register_process(engine_registration(
            "fail-once-artifact-read",
            env_ref,
            serde_json::Value::Null,
        ))
        .await
        .expect("register fail-once engine row");
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("drive failing artifact read");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let record = registry
                .get_process(&artifact_read_retry_record.id)
                .await
                .expect("read process")
                .expect("artifact retry row");
            if record.first_started.is_some()
                && !record.is_terminal()
                && registry
                    .get_process_lease(&artifact_read_retry_record.id)
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

    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("drive retry after artifact recovery");
    await_terminal(&registry, &artifact_read_retry_record.id).await;
    let record = registry
        .get_process(&artifact_read_retry_record.id)
        .await
        .expect("read process")
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
/// started OwnerBound work natively as `Abandoned{OwnerDrain}` under a live lease,
/// while leaving rerunnable, not-yet-started, and other-owner rows untouched.
#[tokio::test]
async fn drain_terminalizes_this_hosts_started_owner_bound_work() {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let owner = local_owner("drain-host", "host-a", "start-a");
    let worker = native_worker(&backend, owner.clone()).await;

    // (a) OwnerBound row this worker started -> drained.
    let mine_started_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register mine-started");
    registry
        .record_first_started(
            &mine_started_record.id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first_started for mine-started");

    // (b) OwnerBound row a DIFFERENT owner started -> not ours to drain.
    let theirs_started_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register theirs-started");
    registry
        .record_first_started(
            &theirs_started_record.id,
            ProcessStarted {
                owner: local_owner("other-host", "host-b", "start-b"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first_started for theirs-started");

    // (c) OwnerBound row never started -> still claimable by anyone.
    let mine_unstarted_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register mine-unstarted");

    // (d) Rerunnable in-flight row this worker started -> left non-terminal for
    // the next worker (its contract; drain never terminalizes rerunnable work).
    let rerunnable_record = registry
        .register_process(registration_with_disposition(RecoveryContract::Rerunnable))
        .await
        .expect("register rerunnable");
    registry
        .record_first_started(
            &rerunnable_record.id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first_started for rerunnable");

    let report = worker.drain_owner_bound_work().await.expect("drain");
    assert_eq!(report.abandoned, vec![mine_started_record.id.clone()]);
    assert!(report.deferred.is_empty());

    let evidence = abandoned_evidence(&registry, &mine_started_record.id).await;
    assert_eq!(evidence.writer, AbandonWriter::OwnerDrain);
    assert_eq!(evidence.owner.as_ref(), Some(&owner));

    for (untouched, process_id) in [
        ("theirs-started", &theirs_started_record.id),
        ("mine-unstarted", &mine_unstarted_record.id),
        ("rerunnable", &rerunnable_record.id),
    ] {
        assert!(
            !registry
                .get_process(process_id)
                .await
                .expect("read process")
                .expect("row exists")
                .is_terminal(),
            "{untouched} must be left non-terminal by owner drain",
        );
    }
}

#[tokio::test]
async fn native_start_records_stable_owner_that_owner_drain_can_match() {
    let started = Arc::new(tokio::sync::Notify::new());
    let fail = Arc::new(tokio::sync::Notify::new());
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (worker, registry, run_handle, env_ref) = worker_with_engine(
        1,
        Arc::new(PausedInfraEngine {
            started: Arc::clone(&started),
            fail: Arc::clone(&fail),
        }),
        run_handle,
    )
    .await;
    let mut registration = engine_registration("paused-infra", env_ref, serde_json::Value::Null);
    registration.disposition = RecoveryContract::OwnerBound;
    let process_id = registry
        .register_process(registration)
        .await
        .expect("register owner-bound engine")
        .id;
    let _ = run_handle
        .enable_and_drive()
        .await
        .expect("drive owner-bound engine");
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("engine starts");

    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("started record");
    assert_eq!(
        record.first_started.as_deref().map(|start| &start.owner),
        Some(&worker.config().lease_owner),
        "durable start fact uses the stable worker owner"
    );

    fail.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
        while registry
            .get_process_lease(&process_id)
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
    assert_eq!(report.abandoned, vec![process_id.to_string()]);
    assert!(report.deferred.is_empty());
}

#[tokio::test]
async fn drain_does_not_report_abandoned_when_terminal_write_fails() {
    let (backend, registry) = faulted_memory_backend().await;
    let owner = local_owner("drain-write-failure", "host-a", "start-a");
    let _process_id = "owner-bound-terminal-write-failure";
    let owner_bound_terminal_write_failure_record = registry
        .register_process(registration_with_disposition(RecoveryContract::OwnerBound))
        .await
        .expect("register owner-bound row");
    let process_id = owner_bound_terminal_write_failure_record.id.clone();
    registry
        .record_first_started(
            &process_id,
            ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
                generation: None,
            },
        )
        .await
        .expect("record first start");
    registry.set_process_terminal_write_error(Some(PluginError::Session(
        "injected terminal-write failure".to_string(),
    )));

    let worker = native_worker(&backend, owner).await;
    let (report, capture) = capturing(|| worker.drain_owner_bound_work()).await;
    let report = report.expect("owner drain");

    assert!(
        report.abandoned.is_empty(),
        "abandoned evidence requires an acknowledged terminal write"
    );
    assert_eq!(
        report.deferred,
        vec![ProcessDrainDeferred {
            process_id: process_id.clone(),
            disposition: ProcessRecoveryAttemptOutcome::BackendError {
                operation: ProcessRecoveryOperation::WriteTerminal,
                error: "plugin session error: injected terminal-write failure".to_string(),
            },
        }]
    );
    assert_recovery_backend_error_event(
        &capture,
        &process_id,
        "write_terminal",
        "plugin session error: injected terminal-write failure",
    );
    assert!(
        !registry
            .get_process(&process_id)
            .await
            .expect("read process")
            .expect("process exists")
            .is_terminal(),
        "the injected store failure is fail-closed"
    );

    registry.set_process_terminal_write_error(None);
    let retry = worker
        .drain_owner_bound_work()
        .await
        .expect("retry owner drain");
    assert_eq!(retry.abandoned, vec![process_id.to_string()]);
    assert!(retry.deferred.is_empty());
}
