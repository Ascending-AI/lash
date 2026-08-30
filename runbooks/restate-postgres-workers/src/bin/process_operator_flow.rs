//! Deterministic PostgreSQL harness for the graceful-drain and
//! request-abandon judged runbooks plus the process-operations selected-drain
//! isolation row.
//!
//! The fixtures use the public Lash facade and durable-process worker surfaces.
//! Process inputs are inert external placeholders where execution is not under
//! test. The selected-drain row uses scripted agent-frame work to exercise the
//! public turn facade without model nondeterminism.

use lash::sync::MutexExt;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use lash::persistence::SessionStoreFactory as _;
use lash::provider::{LlmResponse, ProviderHandle};
use lash::runtime::{
    AwaitEventResolver, ExecutionScope, NativeRuntimeEffectController, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError,
};
use lash_core::{
    AbandonWriter, AwaitEventKey, AwaitEventWaitIdentity, LeaseOwnerIdentity, ProcessAwaitOutput,
    ProcessInput, ProcessListFilter, ProcessProvenance, ProcessRecord, ProcessRegistration,
    ProcessRegistry, ProcessStarted, ProcessStatus, ProcessStatusFilter, RecoveryContract,
    Resolution, ResolveOutcome, SessionScope, TestProcessRegistryWriteExt,
};
use lash_postgres_store::PostgresStorage;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const GATE_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_SESSION_ID: &str = "graceful-drain-in-flight-turn";
const OBSERVER_SESSION_ID: &str = "request-abandon-observer";
const REQUEST_PROCESS_ID: &str = "request-abandon-owner-bound";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let scenario = std::env::args()
        .nth(1)
        .context("usage: lash-e2e-process-operator-flow drain|request-abandon|selected-drain")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .context("connect the runbook PostgreSQL deployment")?;

    match scenario.as_str() {
        "drain" => Box::pin(graceful_drain(&storage)).await,
        "request-abandon" => Box::pin(request_abandon(&storage)).await,
        "selected-drain" => Box::pin(selected_drain_scope_isolation(&storage)).await,
        other => bail!("unknown process operator-flow scenario `{other}`"),
    }
}

fn emit(checkpoint: Value) {
    println!(
        "{}",
        serde_json::to_string(&checkpoint).expect("serialize checkpoint")
    );
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:runbook-incarnation"))
}

fn registration(id: &str, disposition: RecoveryContract) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: json!({"runbook": "process-operator-flow"}),
        },
        disposition,
        ProcessProvenance::host(),
    )
}

async fn register_started(
    registry: &Arc<dyn ProcessRegistry>,
    id: &str,
    disposition: RecoveryContract,
    started_owner: &LeaseOwnerIdentity,
) -> Result<()> {
    registry
        .register_process(registration(id, disposition))
        .await
        .with_context(|| format!("register `{id}`"))?;
    registry
        .record_first_started(
            id,
            ProcessStarted {
                owner: started_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: now_epoch_ms(),
            },
        )
        .await
        .with_context(|| format!("record first start for `{id}`"))?;
    Ok(())
}

/// Records every [`ProcessWorkerFault`] the sweep reports.
///
/// A drive admits rows and returns; a claim, read, terminal write, or lease
/// release that fails afterwards has no other way back to this runbook. The
/// judged scenarios assert the recorded list is empty, so a fault can never
/// hide behind a clean-looking drive.
#[derive(Clone, Default)]
struct RecordingWorkerFaultSink {
    faults: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl lash::process::ProcessEventSink for RecordingWorkerFaultSink {
    async fn emit(&self, _event: &lash::process::ProcessEvent) {}

    async fn emit_worker_fault(&self, fault: &lash::process::ProcessWorkerFault) {
        self.faults.lock_recover().push(format!("{fault:?}"));
    }
}

impl RecordingWorkerFaultSink {
    fn recorded(&self) -> Vec<String> {
        self.faults.lock_recover().clone()
    }
}

fn process_worker(
    storage: &PostgresStorage,
    registry: Arc<dyn ProcessRegistry>,
    lease_owner: LeaseOwnerIdentity,
    fault_sink: &RecordingWorkerFaultSink,
) -> lash::durability::DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let config = lash::durability::DurableProcessWorkerConfig::new(
        Arc::new(lash_core::facade_support::PluginHost::new(Vec::new())),
        lash::durability::RuntimeHostConfig::in_memory(
            lash::CommitBudget::bounded(1024 * 1024, 512),
            lash::QueuedWorkBatchingConfig::new(1024),
        ),
        Arc::new(storage.session_store_factory_with_shared_process_registry()),
        lash::durability::WorkerProcessWork::SelfNative(watched),
        Arc::new(lash::runtime::NoQueuedWork::new()),
        lease_owner,
    )
    .with_trigger_store(Arc::new(storage.trigger_store()))
    .with_process_event_sink(Arc::new(fault_sink.clone()));
    lash::durability::DurableProcessWorker::new(config)
        .expect("runbook worker uses valid native substrate defaults")
}

fn process_json(record: &ProcessRecord) -> Value {
    let (abandon_writer, abandon_owner) = match record.outcome.as_ref() {
        Some(ProcessAwaitOutput::Abandoned { evidence, .. }) => (
            Some(format!("{:?}", evidence.writer)),
            evidence.owner.as_ref().map(|owner| owner.owner_id.clone()),
        ),
        _ => (None, None),
    };
    json!({
        "process_id": record.id,
        "status": format!("{:?}", record.status),
        "terminal": record.is_terminal(),
        "disposition": format!("{:?}", record.disposition),
        "first_started_owner_id": record.first_started.as_ref().map(|started| started.owner.owner_id.clone()),
        "abandon_writer": abandon_writer,
        "abandon_owner_id": abandon_owner,
        "abandon_request": record.abandon_request,
    })
}

/// Read once: a bad `LASH_RUNBOOK_DIALECT` must fail the phase, not one turn.
fn runbook_dialect() -> lash::rlm::RlmDialect {
    static DIALECT: std::sync::OnceLock<lash::rlm::RlmDialect> = std::sync::OnceLock::new();
    *DIALECT.get_or_init(|| {
        lash_restate_postgres_workers_e2e::runbook_rlm_dialect()
            .expect("LASH_RUNBOOK_DIALECT names a registered dialect")
    })
}

/// The scripted cell, in the dialect the row is running. A foreign cell cannot
/// execute, so the turn never reaches a terminal state and the row hangs
/// instead of failing.
fn scripted_response(value: &str) -> LlmResponse {
    let text = lash_restate_postgres_workers_e2e::scripted_finish_cell(
        runbook_dialect(),
        &format!("\"{value}\""),
    );
    LlmResponse {
        parts: vec![lash_core::LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

struct StallingProvider {
    handle: ProviderHandle,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    calls: Arc<AtomicUsize>,
}

impl StallingProvider {
    fn new() -> Self {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            lash_core::testing::TestProvider::builder()
                .kind("process-operator-flow")
                .complete(move |_request| {
                    let entered = Arc::clone(&entered);
                    let release = Arc::clone(&release);
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        release
                            .acquire()
                            .await
                            .expect("the drain harness releases its in-flight provider")
                            .forget();
                        Ok(scripted_response("drained"))
                    }
                })
                .build()
                .into_handle()
        };
        Self {
            handle,
            entered,
            release,
            calls,
        }
    }

    async fn wait_until_in_flight(&self) -> Result<()> {
        tokio::time::timeout(GATE_TIMEOUT, self.entered.notified())
            .await
            .context("provider effect did not enter")?;
        Ok(())
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

/// A controller-owned deterministic journal: active replay keys are the live
/// effect journal, and successful local outcomes move to the completed set.
/// This does not claim workflow-engine persistence; it proves the host waited
/// for the in-flight effect it admitted before declaring the journal empty.
#[derive(Default)]
struct JournalController {
    inline: NativeRuntimeEffectController,
    active: Mutex<BTreeSet<String>>,
    completed: Mutex<BTreeSet<String>>,
}

impl JournalController {
    fn active(&self) -> Vec<String> {
        self.active.lock_recover().iter().cloned().collect()
    }

    fn completed(&self) -> Vec<String> {
        self.completed.lock_recover().iter().cloned().collect()
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for JournalController {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> std::result::Result<AwaitEventKey, RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> std::result::Result<ResolveOutcome, RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> std::result::Result<Option<Resolution>, RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> std::result::Result<Resolution, RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> std::result::Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for JournalController {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: lash_core::RuntimeErrorCode,
    ) -> std::result::Result<lash_core::RuntimeEffectFailureDisposition, lash_core::RuntimeError>
    {
        Ok(lash_core::RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(
        &self,
    ) -> std::result::Result<lash_core::TurnControlParticipation, lash_core::RuntimeError> {
        Ok(lash_core::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let key = envelope
            .invocation
            .replay_key()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{:?}", envelope.invocation.effect_kind()));
        self.active.lock_recover().insert(key.clone());
        let result = self.inline.execute_effect(envelope, local_executor).await;
        self.active.lock_recover().remove(&key);
        if result.is_ok() {
            self.completed.lock_recover().insert(key);
        }
        result
    }
}

fn core(
    storage: &PostgresStorage,
    provider: ProviderHandle,
    attachments: &tempfile::TempDir,
) -> Result<lash::LashCore> {
    let protocol = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(storage.lashlang_artifact_store()),
    );
    lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, protocol)
        .with_native_queued_work()
        .provider(provider)
        .model(
            lash::ModelSpec::builder("process-operator-flow-mock")
                .context_window_tokens(200_000)
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .store_factory(Arc::new(
            storage.session_store_factory_with_shared_process_registry(),
        ))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path().to_path_buf(),
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(storage.process_env_store()))
        .process_registry(Arc::new(storage.process_registry()))
        .trigger_store(Arc::new(storage.trigger_store()))
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "process-operator-flow-worker",
            uuid::Uuid::new_v4().to_string(),
        ))
        .context("build process operator-flow core")
}

fn queued_batch_draft(
    session_id: &str,
    source_key: &str,
    merge_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::QueuedWorkBatchDraft::new(
        session_id,
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        vec![lash::persistence::QueuedWorkPayload::agent_frame_task(
            lash_core::facade_support::frame_node_id(
                session_id,
                "process-operations-selected-drain-frame",
            ),
            source_key,
            None,
        )],
    )
    .with_source_key(source_key)
    .with_merge_key(merge_key)
}

async fn selected_drain_scope_isolation(storage: &PostgresStorage) -> Result<()> {
    const SESSION_ID: &str = "process-operations-selected-drain";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("process-operations-selected-drain")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(scripted_response("selected A only"))
            }
        })
        .build()
        .into_handle();
    let attachments = tempfile::tempdir().context("selected-drain attachment directory")?;
    let core = core(storage, provider, &attachments)?;
    let session = core
        .session(SESSION_ID)
        .plugin_option(
            lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
            lash::rlm::RlmCreateExtras {
                dialect: Some(runbook_dialect()),
                ..lash::rlm::RlmCreateExtras::default()
            },
        )
        .context("state the row's dialect")?
        .open()
        .await?;
    let store_factory = storage.session_store_factory_with_shared_process_registry();
    let store = store_factory
        .create_store(&lash::persistence::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SESSION_ID.to_string(),
            relation: lash::persistence::SessionRelation::Root,
            policy: session.policy_snapshot(),
        })
        .await?;

    let selected_a = store
        .enqueue_queued_work(queued_batch_draft(
            SESSION_ID,
            "selected-drain:a",
            "selected-drain:a",
        ))
        .await?;
    let unselected_b = store
        .enqueue_queued_work(queued_batch_draft(
            SESSION_ID,
            "selected-drain:b",
            "selected-drain:b",
        ))
        .await?;

    let claimed = session
        .queued_turn()
        .batch_ids([selected_a.batch_id.clone()])
        .run()
        .await?;
    ensure!(
        claimed.satisfied
            == vec![lash::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
                batch_id: selected_a.batch_id.clone(),
            }],
        "selected A satisfaction was not exact: {:?}",
        claimed.satisfied
    );
    ensure!(claimed.turn.is_some(), "selected A did not execute a turn");
    ensure!(
        provider_calls.load(Ordering::SeqCst) == 1,
        "selected A executed an unexpected number of provider calls"
    );
    let pending_after_claim = session.queued_work().await?;
    ensure!(
        pending_after_claim
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>()
            == vec![unselected_b.batch_id.as_str()],
        "selected A settled or reordered unselected B: {pending_after_claim:?}"
    );

    let replay = session
        .queued_turn()
        .batch_ids([selected_a.batch_id.clone()])
        .run()
        .await?;
    ensure!(replay.turn.is_none(), "idempotent A replay executed a turn");
    ensure!(
        replay.satisfied
            == vec![
                lash::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                    batch_id: selected_a.batch_id.clone(),
                },
            ],
        "idempotent A replay was not AlreadySatisfied: {:?}",
        replay.satisfied
    );

    let refusal_c1 = store
        .enqueue_queued_work(queued_batch_draft(
            SESSION_ID,
            "selected-drain:c1",
            "selected-drain:c",
        ))
        .await?;
    let refusal_separator = store
        .enqueue_queued_work(queued_batch_draft(
            SESSION_ID,
            "selected-drain:separator",
            "selected-drain:separator",
        ))
        .await?;
    let refusal_c2 = store
        .enqueue_queued_work(queued_batch_draft(
            SESSION_ID,
            "selected-drain:c2",
            "selected-drain:c",
        ))
        .await?;
    let refusal = session
        .queued_turn()
        .batch_ids([refusal_c1.batch_id.clone(), refusal_c2.batch_id.clone()])
        .run()
        .await
        .expect_err("a selection cannot jump one merge key across another");
    let lash::EmbedError::SelectedQueuedWorkDrainRefused {
        cause:
            lash::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                unclaimed_batch_ids,
            },
    } = refusal
    else {
        bail!("selected-drain refusal lost its typed cause: {refusal:?}");
    };
    ensure!(
        unclaimed_batch_ids == vec![refusal_c2.batch_id.clone()],
        "selected-drain refusal named the wrong rows: {unclaimed_batch_ids:?}"
    );
    let pending_after_refusal = session.queued_work().await?;
    let pending_ids = pending_after_refusal
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect::<Vec<_>>();
    ensure!(
        pending_ids
            == vec![
                unselected_b.batch_id.clone(),
                refusal_c1.batch_id.clone(),
                refusal_separator.batch_id.clone(),
                refusal_c2.batch_id.clone(),
            ],
        "typed refusal mutated pending rows: {pending_after_refusal:?}"
    );
    ensure!(
        provider_calls.load(Ordering::SeqCst) == 1,
        "typed refusal reached the provider"
    );

    emit(json!({
        "checkpoint": "selected_drain_scope_isolated",
        "selected_batch_id": selected_a.batch_id,
        "selected_satisfaction": "ClaimedNow",
        "replay_satisfaction": "AlreadySatisfied",
        "unselected_batch_id": unselected_b.batch_id,
        "unselected_pending_after_claim": true,
        "refusal": "UnclaimableTogether",
        "refusal_unclaimed_batch_ids": unclaimed_batch_ids,
        "pending_after_refusal": pending_ids,
        "provider_calls": provider_calls.load(Ordering::SeqCst),
    }));
    Ok(())
}

async fn graceful_drain(storage: &PostgresStorage) -> Result<()> {
    const MINE: &str = "drain-owner-bound-mine";
    const RERUNNABLE: &str = "drain-rerunnable-mine";
    const FOREIGN: &str = "drain-owner-bound-foreign";
    const UNSTARTED: &str = "drain-owner-bound-unstarted";
    const EXTERNAL: &str = "drain-externally-owned";

    let registry: Arc<dyn ProcessRegistry> = Arc::new(storage.process_registry());
    let drain_owner = owner("drain-host");
    let foreign_owner = owner("foreign-host");
    let provider = StallingProvider::new();
    let provider_handle = provider.handle.clone();
    let attachments = tempfile::tempdir().context("drain attachment directory")?;
    let core = core(storage, provider.handle.clone(), &attachments)?;
    let session = {
        core.session(TURN_SESSION_ID)
            .plugin_option(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    dialect: Some(runbook_dialect()),
                    ..lash::rlm::RlmCreateExtras::default()
                },
            )
            .context("state the row's dialect")?
            .open()
            .await?
    };
    let journal = Arc::new(JournalController::default());
    let task_journal = Arc::clone(&journal);
    let turn = tokio::spawn(async move {
        let output = session
            .turn(lash::TurnInput::text("finish the in-flight effect"))
            .turn_id("graceful-drain-in-flight")
            .effects(task_journal.as_ref())
            .run()
            .await;
        (session, output)
    });
    provider.wait_until_in_flight().await?;
    let active_before_drain = journal.active();
    ensure!(
        !active_before_drain.is_empty(),
        "the provider is parked but the controller journal is empty"
    );

    // Seed after the session-open recovery pass has finished. These direct
    // registry writes model work already held by deployment workers; allowing
    // the core's unrelated default worker to claim the never-started row would
    // change the fixture before the drain owner is invoked.
    register_started(&registry, MINE, RecoveryContract::OwnerBound, &drain_owner).await?;
    register_started(
        &registry,
        RERUNNABLE,
        RecoveryContract::Rerunnable,
        &drain_owner,
    )
    .await?;
    register_started(
        &registry,
        FOREIGN,
        RecoveryContract::OwnerBound,
        &foreign_owner,
    )
    .await?;
    registry
        .register_process(registration(UNSTARTED, RecoveryContract::OwnerBound))
        .await?;
    registry
        .register_process(registration(EXTERNAL, RecoveryContract::ExternallyOwned))
        .await?;

    emit(json!({
        "checkpoint": "seeded_drain_deployment",
        "seeded_session_id": TURN_SESSION_ID,
        "in_flight_turn_id": "graceful-drain-in-flight",
        "provider_calls": provider.calls.load(Ordering::SeqCst),
        "journal_active": active_before_drain,
        "ingress_accepting": true,
        "processes": records_json(&registry).await?,
    }));

    // Steps 1-2 are explicitly host policy: close admission, reject a new turn,
    // then let the already-admitted effect settle.
    let ingress_accepting = AtomicBool::new(true);
    ingress_accepting.store(false, Ordering::SeqCst);
    let new_turn_admitted = ingress_accepting.load(Ordering::SeqCst);
    ensure!(
        !new_turn_admitted,
        "host ingress admitted work after quiesce"
    );

    provider.release();
    let (session, output) = tokio::time::timeout(GATE_TIMEOUT, turn)
        .await
        .context("in-flight turn did not settle during drain")?
        .context("in-flight turn task panicked")?;
    let output = output.context("in-flight turn failed")?;
    ensure!(
        output.final_value() == Some(&json!("drained")),
        "in-flight turn did not commit its deterministic terminal"
    );
    let parked = session.park().await.context("park drained session")?;
    let parked_session_id = parked.session_id().to_string();
    ensure!(journal.active().is_empty(), "effect journal is not empty");
    ensure!(
        !journal.completed().is_empty(),
        "no completed effect was recorded"
    );

    // The process worker's run tasks are represented by released leases here;
    // now the worker executes its documented terminal-writing shutdown lever.
    let fault_sink = RecordingWorkerFaultSink::default();
    let worker = process_worker(
        storage,
        Arc::clone(&registry),
        drain_owner.clone(),
        &fault_sink,
    );
    let waiter_core = core.clone();
    let waiter = tokio::spawn(async move { waiter_core.processes().await_output(MINE).await });
    let report = worker.drain_owner_bound_work().await?;
    ensure!(
        report.abandoned == vec![MINE.to_string()],
        "unexpected drain report: {report:?}"
    );
    ensure!(
        report.deferred.is_empty(),
        "owner drain left rows deferred: {report:?}"
    );
    let awaited = tokio::time::timeout(GATE_TIMEOUT, waiter)
        .await
        .context("owner-drain observer did not settle")?
        .context("owner-drain observer task panicked")??;
    let ProcessAwaitOutput::Abandoned { evidence, .. } = awaited else {
        bail!("owner-drain observer saw a non-Abandoned terminal: {awaited:?}");
    };
    ensure!(
        evidence.writer == AbandonWriter::OwnerDrain,
        "wrong drain writer"
    );
    ensure!(
        evidence.owner.as_ref() == Some(&drain_owner),
        "wrong drain owner"
    );

    provider_handle.close().await.context("close provider")?;
    core.flush_trace_sink().context("flush trace sink")?;
    let records = records_json(&registry).await?;
    assert_drain_records(&records)?;
    let drain_faults = fault_sink.recorded();
    ensure!(
        drain_faults.is_empty(),
        "drain reported worker faults: {drain_faults:?}"
    );

    emit(json!({
        "checkpoint": "graceful_drain_observed",
        "drain_worker_faults": drain_faults.len(),
        "ingress_accepting": ingress_accepting.load(Ordering::SeqCst),
        "new_turn_admitted": new_turn_admitted,
        "provider_calls": provider.calls.load(Ordering::SeqCst),
        "in_flight_effect_completed": true,
        "turn_final_value": output.final_value(),
        "parked_session_id": parked_session_id,
        "journal_active": journal.active(),
        "journal_completed": journal.completed(),
        "drain_report_abandoned": report.abandoned,
        "drain_report_deferred": report.deferred.iter().map(|entry| json!({
            "process_id": entry.process_id,
            "disposition": format!("{:?}", entry.disposition),
        })).collect::<Vec<_>>(),
        "observer_terminal": "Abandoned",
        "observer_abandon_writer": format!("{:?}", evidence.writer),
        "observer_abandon_owner_id": evidence.owner.as_ref().map(|owner| owner.owner_id.clone()),
        "provider_closed": true,
        "trace_flushed": true,
        "processes": records,
    }));
    Ok(())
}

async fn records_json(registry: &Arc<dyn ProcessRegistry>) -> Result<Vec<Value>> {
    let mut records = registry.list_processes(&all_processes()).await?;
    records.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(records.iter().map(process_json).collect())
}

fn all_processes() -> ProcessListFilter {
    ProcessListFilter {
        status: ProcessStatusFilter::Any,
        ..ProcessListFilter::default()
    }
}

fn assert_drain_records(records: &[Value]) -> Result<()> {
    for (id, terminal, status) in [
        ("drain-owner-bound-mine", true, "Abandoned"),
        ("drain-rerunnable-mine", false, "Running"),
        ("drain-owner-bound-foreign", false, "Running"),
        ("drain-owner-bound-unstarted", false, "Running"),
        ("drain-externally-owned", false, "Running"),
    ] {
        let row = records
            .iter()
            .find(|row| row["process_id"].as_str() == Some(id))
            .with_context(|| format!("missing drain row `{id}`"))?;
        ensure!(
            row["terminal"].as_bool() == Some(terminal),
            "wrong terminal fact for `{id}`: {row}"
        );
        ensure!(
            row["status"].as_str() == Some(status),
            "wrong status for `{id}`: {row}"
        );
    }
    let mine = records
        .iter()
        .find(|row| row["process_id"].as_str() == Some("drain-owner-bound-mine"))
        .expect("mine row checked above");
    ensure!(
        mine["abandon_writer"].as_str() == Some("OwnerDrain"),
        "wrong evidence: {mine}"
    );
    Ok(())
}

async fn request_abandon(storage: &PostgresStorage) -> Result<()> {
    let registry: Arc<dyn ProcessRegistry> = Arc::new(storage.process_registry());
    let silent_owner = owner("request-abandon-live-owner");
    let sweep_owner = owner("request-abandon-sweeper");
    registry
        .register_process_with_observers(
            registration(REQUEST_PROCESS_ID, RecoveryContract::OwnerBound),
            &[OBSERVER_SESSION_ID.to_string()],
        )
        .await?;
    registry
        .record_first_started(
            REQUEST_PROCESS_ID,
            ProcessStarted {
                owner: silent_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: now_epoch_ms(),
            },
        )
        .await?;
    let live_lease = registry
        .claim_process_lease(REQUEST_PROCESS_ID, &silent_owner, 1_000)
        .await?
        .acquired()
        .context("silent owner did not acquire its lease")?;

    let attachments = tempfile::tempdir().context("request-abandon attachment directory")?;
    let provider = lash_core::testing::TestProvider::builder()
        .kind("process-operator-flow")
        .complete(|_request| async { Ok(scripted_response("unused")) })
        .build()
        .into_handle();
    let core = core(storage, provider, &attachments)?;
    let seeded = core
        .processes()
        .get(REQUEST_PROCESS_ID)
        .await?
        .context("seeded process is not observable")?;
    ensure!(
        seeded.lifecycle == ProcessStatus::Running,
        "seeded row is terminal"
    );
    ensure!(
        seeded.lease_holder.as_ref() == Some(&silent_owner),
        "wrong live holder"
    );

    emit(json!({
        "checkpoint": "seeded_request_abandon_deployment",
        "process_id": REQUEST_PROCESS_ID,
        "status": format!("{:?}", seeded.lifecycle),
        "terminal": seeded.terminal,
        "first_started": seeded.first_started,
        "lease_holder_owner_id": seeded.lease_holder.as_ref().map(|owner| owner.owner_id.clone()),
        "lease_token": live_lease.lease_token,
        "fencing_token": live_lease.fencing_token,
        "lease_expires_at_ms": live_lease.expires_at_epoch_ms,
        "observed_by": OBSERVER_SESSION_ID,
    }));

    let returned = core
        .processes()
        .request_abandon(
            REQUEST_PROCESS_ID,
            "runbook-operator",
            Some("owner retired during operator exercise".to_string()),
        )
        .await?;
    let pending_request = returned
        .abandon_request
        .as_ref()
        .context("request_abandon returned no marker")?;
    ensure!(
        returned.lifecycle == ProcessStatus::Running,
        "request terminalized the row"
    );
    let lease_after_request = registry
        .get_process_lease(REQUEST_PROCESS_ID)
        .await?
        .context("request removed the live owner lease")?;
    ensure!(
        serde_json::to_value(&lease_after_request)? == serde_json::to_value(&live_lease)?,
        "request mutated the live owner lease"
    );
    let observed_pending = core
        .processes()
        .list_observed_by(&SessionScope::new(OBSERVER_SESSION_ID), &all_processes())
        .await?;
    ensure!(
        observed_pending.iter().any(|process| {
            process.process_id == REQUEST_PROCESS_ID
                && process.lifecycle == ProcessStatus::Running
                && process.abandon_request.as_ref() == Some(pending_request)
        }),
        "observer did not see the pending marker"
    );

    emit(json!({
        "checkpoint": "pending_abandon_request_visible",
        "process_id": REQUEST_PROCESS_ID,
        "returned_status": format!("{:?}", returned.lifecycle),
        "returned_terminal": returned.terminal,
        "requested_by": pending_request.requested_by,
        "requested_at_ms": pending_request.requested_at_ms,
        "reason": pending_request.reason,
        "observer_marker_visible": true,
        "observer_count": observed_pending.len(),
        "lease_unchanged": true,
        "lease_holder_owner_id": lease_after_request.owner.owner_id,
        "lease_token": lease_after_request.lease_token,
        "fencing_token": lease_after_request.fencing_token,
        "lease_expires_at_ms": lease_after_request.expires_at_epoch_ms,
    }));

    // Poll the authoritative persisted lease until it is observably lapsed.
    let deadline = tokio::time::Instant::now() + GATE_TIMEOUT;
    let lapsed_observation = loop {
        let observed = core
            .processes()
            .get(REQUEST_PROCESS_ID)
            .await?
            .context("pending process vanished")?;
        if observed
            .lease_expires_at_ms
            .is_some_and(|expires| expires <= now_epoch_ms())
        {
            break observed;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "owner lease did not lapse within {GATE_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    ensure!(
        lapsed_observation.lifecycle == ProcessStatus::Running,
        "lease lapse terminalized the row before the sweep"
    );

    let fault_sink = RecordingWorkerFaultSink::default();
    let worker = process_worker(storage, Arc::clone(&registry), sweep_owner, &fault_sink);
    let admission = worker.drive_pending_processes().await?;
    let terminal = loop {
        let observed = core
            .processes()
            .get(REQUEST_PROCESS_ID)
            .await?
            .context("reconciled process vanished")?;
        if observed.terminal {
            break observed;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "sweep did not reconcile the request within {GATE_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    ensure!(
        terminal.lifecycle == ProcessStatus::Abandoned,
        "wrong terminal status"
    );
    let awaited = core.processes().await_output(REQUEST_PROCESS_ID).await?;
    let ProcessAwaitOutput::Abandoned { evidence, .. } = awaited else {
        bail!("await_output returned a non-Abandoned terminal: {awaited:?}");
    };
    ensure!(
        evidence.writer == AbandonWriter::ReconciledRequest,
        "wrong reconciliation writer"
    );
    ensure!(
        evidence.owner.as_ref() == Some(&silent_owner),
        "wrong lapsed owner"
    );
    ensure!(
        registry
            .get_process_lease(REQUEST_PROCESS_ID)
            .await?
            .is_none(),
        "reconciled terminal retained a lease"
    );
    let observed_terminal = core
        .processes()
        .list_observed_by(&SessionScope::new(OBSERVER_SESSION_ID), &all_processes())
        .await?;
    ensure!(
        observed_terminal.iter().any(|process| {
            process.process_id == REQUEST_PROCESS_ID
                && process.lifecycle == ProcessStatus::Abandoned
                && process.terminal
        }),
        "observer did not see the reconciled terminal"
    );
    let sweep_faults = fault_sink.recorded();
    ensure!(
        sweep_faults.is_empty(),
        "sweep reported worker faults: {sweep_faults:?}"
    );

    emit(json!({
        "checkpoint": "abandon_request_reconciled",
        "sweep_admitted": admission.admitted.len(),
        "sweep_worker_faults": sweep_faults.len(),
        "process_id": REQUEST_PROCESS_ID,
        "lapsed_before_sweep_status": format!("{:?}", lapsed_observation.lifecycle),
        "lapsed_before_sweep_terminal": lapsed_observation.terminal,
        "lapsed_lease_expires_at_ms": lapsed_observation.lease_expires_at_ms,
        "observed_after_expiry_at_ms": now_epoch_ms(),
        "terminal_status": format!("{:?}", terminal.lifecycle),
        "terminal": terminal.terminal,
        "abandon_writer": format!("{:?}", evidence.writer),
        "lapsed_owner_id": evidence.owner.as_ref().map(|owner| owner.owner_id.clone()),
        "observer_terminal_visible": true,
        "observer_count": observed_terminal.len(),
        "lease_cleared": true,
        "pending_marker_retained_on_terminal": terminal.abandon_request.is_some(),
    }));
    Ok(())
}
