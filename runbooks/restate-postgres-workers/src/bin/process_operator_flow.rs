//! Deterministic PostgreSQL harness for the graceful-drain and
//! request-abandon judged runbooks plus the process-operations selected-drain
//! isolation row.
//!
//! The fixtures use the public Lash facade and durable-process worker surfaces.
//! Process inputs are inert external placeholders where execution is not under
//! test. The selected-drain row uses scripted agent-frame work to exercise the
//! public turn facade without model nondeterminism.

use lash::ProcessId;
use lash::SessionId;
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
    Resolution, ResolveOutcome, SessionScope,
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

#[expect(
    clippy::expect_used,
    reason = "checkpoints are serde Values that always serialize"
)]
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
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

async fn record_started(
    registry: &Arc<dyn ProcessRegistry>,
    id: &str,
    started: ProcessStarted,
) -> Result<()> {
    let owner = LeaseOwnerIdentity::opaque(format!("test-fixture:{id}"), "lifecycle-write");
    let lease = registry
        .claim_process_lease(&ProcessId::from(id), &owner, 60_000)
        .await?
        .acquired()
        .with_context(|| format!("claim setup lease for `{id}`"))?;
    let result = registry
        .record_first_started_with_authority(
            &ProcessId::from(id),
            started,
            &lash_core::ProcessExecutionWriteAuthority::lease(lease.clone()),
        )
        .await;
    let release = registry
        .complete_process_lease(&lash_core::ProcessLeaseCompletion::from_lease(&lease))
        .await;
    match result? {
        lash_core::ProcessStartOutcome::Started(_)
        | lash_core::ProcessStartOutcome::AlreadyApplied(_) => {}
        refused => bail!("record first start for `{id}` was refused: {refused:?}"),
    }
    release?;
    Ok(())
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
    record_started(
        registry,
        id,
        ProcessStarted {
            owner: started_owner.clone(),
            fencing_token: 0,
            attempt: 1,
            started_at_ms: now_epoch_ms(),
            replay_grammar: None,
        },
    )
    .await
    .with_context(|| format!("record first start for `{id}`"))?;
    Ok(())
}

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

#[expect(
    clippy::expect_used,
    reason = "the runbook worker config supplies bounded commit-budget and batching values; \
              DurableProcessWorker::new refuses only unvalidated configs"
)]
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

/// The scripted cell. A cell the session cannot execute never reaches a
/// terminal state, so the row would hang instead of failing.
fn scripted_response(value: &str) -> LlmResponse {
    let text = lash_restate_postgres_workers_e2e::scripted_finish_cell(&format!("\"{value}\""));
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
    /// Counts the host's `close` calls, so the drain step reports the closes
    /// the provider observed rather than asserting its own intention.
    closes: Arc<AtomicUsize>,
}

impl StallingProvider {
    #[expect(
        clippy::expect_used,
        reason = "the harness releases the stalling provider (see the release semantics call \
                  site), so the acquire cannot time out"
    )]
    fn new() -> Self {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let handle = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            let closes = Arc::clone(&closes);
            lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
                .kind("process-operator-flow")
                .on_close(move || {
                    closes.fetch_add(1, Ordering::SeqCst);
                })
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
            closes,
        }
    }

    /// Wait for the provider to park, but never outlive the turn that is
    /// supposed to reach it. A turn that fails or finishes first would
    /// otherwise leave this gate waiting for a notification nobody will send,
    /// and the real error would be discarded with the task: every such defect
    /// reported itself only as `provider effect did not enter: deadline has
    /// elapsed`.
    async fn wait_until_in_flight<S, E>(
        &self,
        turn: &mut tokio::task::JoinHandle<(S, std::result::Result<lash::TurnOutput, E>)>,
    ) -> Result<()>
    where
        S: Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        tokio::select! {
            entered = tokio::time::timeout(GATE_TIMEOUT, self.entered.notified()) => {
                entered.context("provider effect did not enter")?;
                Ok(())
            }
            joined = &mut *turn => {
                let (_session, output) = joined.context("in-flight turn task panicked")?;
                match output {
                    Ok(output) => bail!(
                        "the in-flight turn ended before entering the provider effect: {:?}",
                        output.result.outcome
                    ),
                    Err(err) => Err(anyhow::Error::new(err))
                        .context("in-flight turn failed before entering the provider effect"),
                }
            }
        }
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
        session_id: &SessionId,
    ) -> std::result::Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for JournalController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let key = envelope.invocation.replay_key().to_owned();
        self.active.lock_recover().insert(key.clone());
        let result = self.inline.execute_effect(envelope, local_executor).await;
        self.active.lock_recover().remove(&key);
        if result.is_ok() {
            self.completed.lock_recover().insert(key);
        }
        result
    }

    async fn open_effect_group(
        &self,
        group: lash::runtime::RuntimeEffectGroup,
    ) -> Result<lash::runtime::EffectGroupHandle, lash::runtime::RuntimeEffectControllerError> {
        self.inline.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash::runtime::EffectGroupHandle,
        cancel: lash::CancellationToken,
    ) -> Result<lash::runtime::GroupSettlement, lash::runtime::RuntimeEffectControllerError> {
        self.inline.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash::runtime::EffectGroupHandle,
        disposition: lash::runtime::LoserPolicy,
    ) -> Result<(), lash::runtime::RuntimeEffectControllerError> {
        self.inline.close_effect_group(handle, disposition).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash::runtime::RuntimeEffectControllerError,
    > {
        self.inline.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash::runtime::RuntimeEffectControllerError> {
        self.inline
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

/// Every scenario writes a JSONL trace. A drain step that claims it flushed
/// the sink has to read the flushed records back, and `flush_trace_sink` is a
/// no-op when no sink is configured, so the sink is part of the shared core
/// rather than a per-scenario extra.
fn core(
    storage: &PostgresStorage,
    provider: ProviderHandle,
    attachments: &tempfile::TempDir,
    trace_path: &std::path::Path,
) -> Result<lash::LashCore> {
    let protocol = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
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
        .trace_jsonl_path(trace_path)
        .trace_level(lash::tracing::TraceLevel::Extended)
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "process-operator-flow-worker",
            uuid::Uuid::new_v4().to_string(),
        ))
        .context("build process operator-flow core")
}

/// Where a scenario writes its trace. The gate reads back the same file the
/// harness read, so when the gate names a directory the trace has to outlive
/// this process; a direct run keeps its own temporary directory instead.
enum TraceRoot {
    Owned(tempfile::TempDir),
    Given(std::path::PathBuf),
}

impl TraceRoot {
    fn open(label: &str) -> Result<Self> {
        match std::env::var("LASH_PROCESS_OPERATOR_TRACE_DIR") {
            Ok(dir) if !dir.is_empty() => {
                let path = std::path::PathBuf::from(dir);
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create the {label} trace directory"))?;
                Ok(Self::Given(path))
            }
            _ => Ok(Self::Owned(tempfile::tempdir().with_context(|| {
                format!("open a temporary {label} trace directory")
            })?)),
        }
    }

    fn path(&self) -> &std::path::Path {
        match self {
            Self::Owned(dir) => dir.path(),
            Self::Given(path) => path.as_path(),
        }
    }
}

/// The dialect the turn's committed cell ran under, read off the RLM
/// extraction diagnostic that cell committed. That record names the dialect
/// twice and never as a constant of this harness: `llm_extraction_payload`
/// builds the counts key as `{language_id}_cell_count` and the decision as
/// the dialect's own `execute_<language>` name. ADR 0096 leaves one language
/// in the tree, which is exactly why a literal here and a second literal in
/// the gate would agree with each other while agreeing with nothing the turn
/// did.
fn recorded_dialect(records: &[Value]) -> Result<String> {
    for record in records {
        if record.get("type").and_then(Value::as_str) != Some("protocol_step")
            || record
                .pointer("/payload/RlmDiagnostic/phase")
                .and_then(Value::as_str)
                != Some("llm_extraction")
        {
            continue;
        }
        let payload = record
            .pointer("/payload/RlmDiagnostic/payload")
            .context("an extraction diagnostic carried no payload")?;
        let counts = payload
            .get("counts")
            .and_then(Value::as_object)
            .context("an extraction diagnostic carried no counts")?;
        let Some(dialect) = counts.iter().find_map(|(key, value)| {
            let dialect = key.strip_suffix("_cell_count")?;
            (value.as_u64()? > 0).then(|| dialect.to_string())
        }) else {
            continue;
        };
        let decision = payload.get("decision").and_then(Value::as_str);
        ensure!(
            decision == Some(format!("execute_{dialect}").as_str()),
            "the extraction counted a {dialect} cell but decided {decision:?}"
        );
        return Ok(dialect);
    }
    bail!("the flushed trace records no executed cell to read a dialect from")
}

/// The trace records the core flushed, as a judge reads them off disk. A
/// missing file is an empty flush rather than an error: the gate's reading is
/// the count, and zero is a failing count, not a crash.
fn flushed_trace_records(path: &std::path::Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("read the flushed trace"),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parse a flushed trace record"))
        .collect()
}

fn queued_batch_draft(
    session_id: &SessionId,
    source_key: &str,
    merge_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::QueuedWorkBatchDraft::new(
        session_id,
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        lash::persistence::TurnWorkPayload::agent_frame_task(
            lash_core::facade_support::frame_node_id(
                session_id,
                "process-operations-selected-drain-frame",
            ),
            source_key,
            None,
        ),
    )
    .with_source_key(source_key)
    .with_merge_key(merge_key)
}

#[expect(
    clippy::expect_used,
    reason = "the wired batch-id selection must be refused: it cannot jump one merge key \
              across another"
)]
async fn selected_drain_scope_isolation(storage: &PostgresStorage) -> Result<()> {
    const SESSION_ID: &str = "process-operations-selected-drain";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider =
        lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
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
    let traces = TraceRoot::open("selected-drain")?;
    let core = core(
        storage,
        provider,
        &attachments,
        &traces.path().join("selected-drain.trace.jsonl"),
    )?;
    let session = core.session(SESSION_ID).open().await?;
    let store_factory = storage.session_store_factory_with_shared_process_registry();
    let store = store_factory
        .create_store(&lash::persistence::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(SESSION_ID.to_string()),
            relation: lash::persistence::SessionRelation::Root,
            policy: session.policy_snapshot(),
        })
        .await?;

    let selected_a = store
        .enqueue_queued_work(queued_batch_draft(
            &SessionId::from(SESSION_ID),
            "selected-drain:a",
            "selected-drain:a",
        ))
        .await?;
    let unselected_b = store
        .enqueue_queued_work(queued_batch_draft(
            &SessionId::from(SESSION_ID),
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
    let pending_after_claim = session.durable().queued_work().await?;
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
            &SessionId::from(SESSION_ID),
            "selected-drain:c1",
            "selected-drain:c",
        ))
        .await?;
    let refusal_separator = store
        .enqueue_queued_work(queued_batch_draft(
            &SessionId::from(SESSION_ID),
            "selected-drain:separator",
            "selected-drain:separator",
        ))
        .await?;
    let refusal_c2 = store
        .enqueue_queued_work(queued_batch_draft(
            &SessionId::from(SESSION_ID),
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
    let pending_after_refusal = session.durable().queued_work().await?;
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
    let traces = TraceRoot::open("drain")?;
    let trace_path = traces.path().join("graceful-drain.trace.jsonl");
    let core = core(storage, provider.handle.clone(), &attachments, &trace_path)?;
    let session = { core.session(TURN_SESSION_ID).open().await? };
    let journal = Arc::new(JournalController::default());
    let task_journal = Arc::clone(&journal);
    let mut turn = tokio::spawn(async move {
        let output = session
            .turn(lash::TurnInput::text("finish the in-flight effect"))
            .turn_id("graceful-drain-in-flight")
            .run_with_effects(task_journal.as_ref())
            .await;
        (session, output)
    });
    provider.wait_until_in_flight(&mut turn).await?;
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
    let journal_completed = journal.completed();
    ensure!(
        !journal_completed.is_empty(),
        "no completed effect was recorded"
    );
    // The step under test is "let the admitted effect settle", so the reading
    // is which of the effects that were in flight before the drain are in the
    // controller's completed set afterwards, not a host-authored `true`.
    let in_flight_effect_completed = active_before_drain
        .iter()
        .filter(|effect| journal_completed.contains(effect))
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        in_flight_effect_completed == active_before_drain,
        "the in-flight effect did not settle during drain: before={active_before_drain:?} \
         completed={journal_completed:?}"
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
    let waiter = tokio::spawn(async move {
        waiter_core
            .processes()
            .await_output(&ProcessId::from(MINE))
            .await
    });
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
    let provider_closed = provider.closes.load(Ordering::SeqCst);
    ensure!(provider_closed == 1, "the provider observed no close");
    core.flush_trace_sink().context("flush trace sink")?;
    let trace_records = flushed_trace_records(&trace_path)?;
    ensure!(
        !trace_records.is_empty(),
        "the flushed trace sink recorded nothing at {}",
        trace_path.display()
    );
    let dialect = recorded_dialect(&trace_records)?;
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
        "dialect": dialect,
        "trace_path": trace_path,
        "in_flight_effect_completed": in_flight_effect_completed,
        "turn_final_value": output.final_value(),
        "parked_session_id": parked_session_id,
        "journal_active": journal.active(),
        "journal_completed": journal_completed,
        "drain_report_abandoned": report.abandoned,
        "drain_report_deferred": report.deferred.iter().map(|entry| json!({
            "process_id": entry.process_id,
            "disposition": format!("{:?}", entry.disposition),
        })).collect::<Vec<_>>(),
        "observer_terminal": "Abandoned",
        "observer_abandon_writer": format!("{:?}", evidence.writer),
        "observer_abandon_owner_id": evidence.owner.as_ref().map(|owner| owner.owner_id.clone()),
        "provider_closed": provider_closed,
        "trace_flushed": trace_records.len(),
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

#[expect(
    clippy::expect_used,
    reason = "the per-row assertions above prove the `drain-owner-bound-mine` row is present in \
              these records"
)]
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
            &[SessionId::from(OBSERVER_SESSION_ID.to_string())],
        )
        .await?;
    record_started(
        &registry,
        REQUEST_PROCESS_ID,
        ProcessStarted {
            owner: silent_owner.clone(),
            fencing_token: 0,
            attempt: 1,
            started_at_ms: now_epoch_ms(),
            replay_grammar: None,
        },
    )
    .await?;
    let live_lease = registry
        .claim_process_lease(&ProcessId::from(REQUEST_PROCESS_ID), &silent_owner, 1_000)
        .await?
        .acquired()
        .context("silent owner did not acquire its lease")?;

    let attachments = tempfile::tempdir().context("request-abandon attachment directory")?;
    let provider =
        lash_restate_postgres_workers_e2e::scripted_provider::ScriptedProvider::builder()
            .kind("process-operator-flow")
            .complete(|_request| async { Ok(scripted_response("unused")) })
            .build()
            .into_handle();
    let traces = TraceRoot::open("request-abandon")?;
    let core = core(
        storage,
        provider,
        &attachments,
        &traces.path().join("request-abandon.trace.jsonl"),
    )?;
    let seeded = core
        .processes()
        .get(&ProcessId::from(REQUEST_PROCESS_ID))
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
        "terminal": seeded.terminal(),
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
            &ProcessId::from(REQUEST_PROCESS_ID),
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
        .get_process_lease(&ProcessId::from(REQUEST_PROCESS_ID))
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
        "returned_terminal": returned.terminal(),
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
            .get(&ProcessId::from(REQUEST_PROCESS_ID))
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
            .get(&ProcessId::from(REQUEST_PROCESS_ID))
            .await?
            .context("reconciled process vanished")?;
        if observed.terminal() {
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
    let awaited = core
        .processes()
        .await_output(&ProcessId::from(REQUEST_PROCESS_ID))
        .await?;
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
    // Re-read the lease rather than asserting the cleanup happened: the
    // holder the registry still reports is the reading the gate needs.
    let reconciled_lease = registry
        .get_process_lease(&ProcessId::from(REQUEST_PROCESS_ID))
        .await?;
    ensure!(
        reconciled_lease.is_none(),
        "reconciled terminal retained a lease: {reconciled_lease:?}"
    );
    let observed_terminal = core
        .processes()
        .list_observed_by(&SessionScope::new(OBSERVER_SESSION_ID), &all_processes())
        .await?;
    let observed_row = observed_terminal
        .iter()
        .find(|process| process.process_id == REQUEST_PROCESS_ID)
        .context("observer did not see the reconciled row at all")?;
    ensure!(
        observed_row.lifecycle == ProcessStatus::Abandoned && observed_row.terminal(),
        "observer did not see the reconciled terminal: {observed_row:?}"
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
        "lapsed_before_sweep_terminal": lapsed_observation.terminal(),
        "lapsed_lease_expires_at_ms": lapsed_observation.lease_expires_at_ms,
        "observed_after_expiry_at_ms": now_epoch_ms(),
        "terminal_status": format!("{:?}", terminal.lifecycle),
        "terminal": terminal.terminal(),
        "abandon_writer": format!("{:?}", evidence.writer),
        "lapsed_owner_id": evidence.owner.as_ref().map(|owner| owner.owner_id.clone()),
        "observer_terminal_status": format!("{:?}", observed_row.lifecycle),
        "observer_terminal_terminal": observed_row.terminal(),
        "observer_count": observed_terminal.len(),
        "reconciled_lease_holder": reconciled_lease
            .as_ref()
            .map(|lease| lease.owner.owner_id.clone()),
        "pending_marker_retained_on_terminal": terminal.abandon_request.is_some(),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extraction_diagnostic(counts: Value, decision: &str) -> Value {
        json!({
            "schema_version": 22,
            "type": "protocol_step",
            "plugin_id": "rlm_protocol",
            "payload": {
                "RlmDiagnostic": {
                    "phase": "llm_extraction",
                    "payload": {
                        "decision": decision,
                        "termination": "natural",
                        "counts": counts,
                    },
                }
            },
        })
    }

    /// The dialect the gate compares against is read from the record the turn
    /// committed, so a harness that never ran a cell reports that rather than
    /// a language name it knew before the turn started.
    #[test]
    fn the_dialect_is_read_from_the_committed_cells_own_diagnostic() {
        let records = vec![
            json!({ "schema_version": 22, "type": "turn_started" }),
            extraction_diagnostic(
                json!({ "code_chars": 18, "typescript_cell_count": 1 }),
                "execute_typescript",
            ),
        ];
        assert_eq!(
            recorded_dialect(&records).expect("a committed cell names its dialect"),
            "typescript"
        );
    }

    #[test]
    fn a_trace_with_no_executed_cell_names_no_dialect() {
        let records = vec![
            json!({ "schema_version": 22, "type": "turn_started" }),
            extraction_diagnostic(
                json!({ "code_chars": 0, "typescript_cell_count": 0 }),
                "stop_no_progress",
            ),
        ];
        let error = recorded_dialect(&records).expect_err("no cell ran");
        assert!(
            error.to_string().contains("no executed cell"),
            "unexpected error: {error}"
        );
    }

    /// The two spellings in the record come from the same dialect, so a record
    /// whose decision disagrees with the cell it counted is a defect in the
    /// reading, not a dialect to report.
    #[test]
    fn a_decision_that_disagrees_with_the_counted_cell_is_refused() {
        let records = vec![extraction_diagnostic(
            json!({ "code_chars": 18, "typescript_cell_count": 1 }),
            "execute_lashlang",
        )];
        let error = recorded_dialect(&records).expect_err("the record disagrees with itself");
        assert!(
            error.to_string().contains("but decided"),
            "unexpected error: {error}"
        );
    }

    /// A trace file the core never wrote is an empty flush, and the drain step
    /// reports a count of zero rather than failing to read its own evidence.
    #[test]
    fn an_unwritten_trace_reads_as_an_empty_flush() {
        let directory = tempfile::tempdir().expect("trace directory");
        assert!(
            flushed_trace_records(&directory.path().join("absent.trace.jsonl"))
                .expect("an unwritten trace reads")
                .is_empty()
        );
    }

    /// A controller that answers `Journaled` must also name the durable
    /// authority that minted its await-event keys, or the runtime refuses the
    /// turn with `invalid_turn_cancel_request` before it reaches the provider
    /// (#1226). This fixture's journal wraps the in-process native controller
    /// and owns no durable authority, so the two answers have to stay
    /// coherent: claiming durable turn control here is what made the drain
    /// flow fail with nothing but `provider effect did not enter`.
    #[test]
    fn the_drain_journal_never_claims_durable_turn_control_without_an_authority() {
        let journal = JournalController::default();
        if journal.effect_journaling() == lash_core::EffectJournaling::Journaled {
            assert!(
                journal.await_event_authority_binding_id().is_some(),
                "a durable-journaled controller must name its await-event authority"
            );
        }
    }

    /// The in-flight gate must never outlive the turn it waits on. A turn that
    /// fails before reaching the provider used to leave the gate parked on a
    /// notification nobody would send, so every such defect reported itself
    /// only as `provider effect did not enter: deadline has elapsed` after the
    /// full 30 s budget.
    #[tokio::test]
    async fn the_in_flight_gate_reports_a_turn_that_failed_before_the_provider() {
        let provider = StallingProvider::new();
        let mut turn = tokio::spawn(async {
            let failure: std::result::Result<lash::TurnOutput, std::io::Error> = Err(
                std::io::Error::other("the turn failed before entering the provider"),
            );
            ((), failure)
        });
        let error = provider
            .wait_until_in_flight(&mut turn)
            .await
            .expect_err("a failed turn must fail the gate");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("failed before entering the provider effect"),
            "the gate must name the turn's own failure, got: {rendered}"
        );
        assert!(
            rendered.contains("the turn failed before entering the provider"),
            "the gate must carry the turn's error, got: {rendered}"
        );
    }
}
