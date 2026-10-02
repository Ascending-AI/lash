use anyhow::{Context, Result, bail};
use lash::ProcessId;
use lash::SessionId;
use lash::durability::EffectHost as _;
use lash::persistence::{
    DeliveryPolicy, DeploymentStore, PROCESS_WAKE_MERGE_KEY, QueuedWorkBatchDraft,
    QueuedWorkStore as _, SessionCatalogStore as _, SessionCreationHead, SessionRelation,
    SessionStoreCreateRequest,
};
use lash::postgres::{PostgresStorage, PostgresStoreSet};
use lash::process::{
    AdmittedProcessIdentity, Lifetime, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessIdentity, ProcessInput, ProcessListFilter,
    ProcessProvenance, ProcessRegistration, ProcessRegistry, ProcessStatusFilter,
    ProcessValueSelector, ProcessWakeDelivery, ProcessWakeSpec, WakeDeliveryConfig,
    WakeDeliveryDriver, WakeDeliveryState, WakeDiscardReason, process_wake_source_key,
};
use lash::runtime::{AdmittedScope, SessionPolicy, SystemClock};
use lash_restate_postgres_workers_e2e::process_operations::{
    self, ReplacementBaseline, StatePlugin,
};
use restate_sdk::prelude::{HandlerResult, Json, WorkflowContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const PROCESS_ID: &str = "process-operations-crash-recovery";
const SESSION_ID: &str = "process-operations-crash-target";
const EVENT_TYPE: &str = "runbook.wake";

#[derive(Serialize, Deserialize)]
enum ReplacementRequest {
    Prepare,
    Recover(Box<ReplacementBaseline>),
}

#[derive(Clone)]
struct ProcessOperationsReplacement {
    core: lash::LashCore,
    plugin: StatePlugin,
    authority: lash::restate::RestateAuthorityId,
}

struct ReplacementEndpoint(tokio::task::JoinHandle<()>);

impl Drop for ReplacementEndpoint {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn replacement_handler_error(error: anyhow::Error) -> restate_sdk::errors::HandlerError {
    if error
        .downcast_ref::<lash::EmbedError>()
        .is_some_and(lash::EmbedError::is_retryable)
    {
        error.into()
    } else {
        restate_sdk::errors::TerminalError::new(error.to_string()).into()
    }
}

#[restate_sdk::workflow]
impl ProcessOperationsReplacement {
    #[restate_sdk::handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<ReplacementRequest>,
    ) -> HandlerResult<Json<Option<ReplacementBaseline>>> {
        let controller = lash::restate::RestateRuntimeEffectController::new(
            ctx,
            self.authority.clone(),
            self.core.build_generation().clone(),
        );
        let scoped = controller
            .scoped(AdmittedScope::runtime_operation(controller.context().key()))
            .map_err(|error| replacement_handler_error(error.into()))?;
        let outcome = match request {
            ReplacementRequest::Prepare => {
                process_operations::prepare(&self.core, &self.plugin, scoped)
                    .await
                    .map(Some)
            }
            ReplacementRequest::Recover(before) => {
                process_operations::recover(&self.core, &self.plugin, &before, scoped)
                    .await
                    .map(|()| None)
            }
        };
        outcome.map(Json).map_err(replacement_handler_error)
    }
}

async fn replacement(storage: &PostgresStorage, mode: &str) -> Result<()> {
    use lash_restate_postgres_workers_e2e::local_restate::LocalRestate;

    let restate = LocalRestate::from_env()?;
    let artifacts = std::path::PathBuf::from(
        std::env::var("LASH_PROCESS_OPERATIONS_ARTIFACT_DIR").context("replacement artifacts")?,
    );
    let attachments =
        lash::sqlite::SqliteStoreSet::open(artifacts.join("replacement-attachments")).await?;
    let stores = Arc::new(PostgresStoreSet::new(
        storage,
        attachments.attachment_store(),
    ));
    let engine = restate.engine(stores);
    let plugin = StatePlugin::default();
    let core = process_operations::core(lash::Backend::new(engine.clone()), plugin.clone())?;
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)?;
    let endpoint = engine
        .endpoint_builder(worker)?
        .bind(ProcessOperationsReplacement {
            core: core.clone(),
            plugin,
            authority: restate.authority.clone(),
        })
        .build();
    let listener =
        tokio::net::TcpListener::bind(std::env::var("LASH_PROCESS_OPERATIONS_ENDPOINT")?).await?;
    let deployment = ReplacementEndpoint(tokio::spawn(async move {
        lash::restate::serve_endpoint(
            listener,
            endpoint,
            lash::restate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
            std::future::pending::<()>(),
        )
        .await;
    }));
    engine
        .register_deployment(&std::env::var(
            "LASH_PROCESS_OPERATIONS_ADVERTISED_ENDPOINT",
        )?)
        .await?;
    let evidence_file = artifacts.join("08-replacement-baseline.json");
    let request = if mode == "replacement-prepare" {
        ReplacementRequest::Prepare
    } else {
        ReplacementRequest::Recover(serde_json::from_slice(&std::fs::read(&evidence_file)?)?)
    };
    let ingress = lash::restate::RestateIngressClient::new(restate.ingress_url);
    let before: Option<ReplacementBaseline> = tokio::time::timeout(
        Duration::from_secs(120),
        ingress.call_workflow_json("ProcessOperationsReplacement", mode, "run", &request),
    )
    .await
    .context("replacement phase timed out")??;
    if let Some(before) = before {
        std::fs::write(evidence_file, serde_json::to_vec_pretty(&before)?)?;
    }
    core.shutdown().await?;
    drop(deployment);
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "the runbook's fixed wake-expiry and stale-claim ages satisfy WakeDeliveryConfig's \
             validation bounds"
)]
fn registry(storage: &PostgresStorage) -> Arc<dyn ProcessRegistry> {
    Arc::new(
        storage.process_registry_with_wake_delivery_config(
            WakeDeliveryConfig::new(60_000)
                .expect("valid runbook wake expiry")
                .with_enqueuing_stale_after_ms(1)
                .expect("valid runbook stale-claim age"),
        ),
    )
}

/// The crash-recovery process, found by its label: the registrar mints its
/// id, and the runbook's later invocations run in fresh processes that were
/// never handed it.
async fn crash_recovery_process(registry: &dyn ProcessRegistry) -> Result<ProcessId> {
    registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .context("list runbook processes")?
        .into_iter()
        .find(|record| record.identity.label.as_deref() == Some(PROCESS_ID))
        .map(|record| record.id)
        .context("the crash-recovery process is not registered")
}

fn registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: json!({"runbook": "process-operations"}),
        },
        ProcessProvenance::host(),
        Lifetime::Detached,
    )
    .with_admitted_identity(AdmittedProcessIdentity::pinned(
        ProcessIdentity::for_definition(
            lash::process::ProcessDefinitionRef::unclaimed(
                "runbook",
                json!({"scenario": "worker-crash-recovery"}),
            ),
            Some(PROCESS_ID),
        ),
    ))
    .with_extra_event_types([ProcessEventType {
        name: EVENT_TYPE.to_string(),
        payload_schema: lash::triggers::JsonSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: None,
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }])
    .with_wake_session_id(Some(SessionId::from(SESSION_ID)))
}

async fn process_events(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
) -> Result<Vec<ProcessEvent>> {
    let limit = std::num::NonZeroUsize::new(256).unwrap_or(std::num::NonZeroUsize::MIN);
    let mut after_sequence = 0;
    let mut events = Vec::new();
    loop {
        let outcome = registry
            .event_page_after(
                process_id,
                after_sequence,
                limit,
                ProcessEventQueryMode::Full,
            )
            .await
            .context("read process event page")?;
        let ProcessEventReadOutcome::Retained(page) = outcome else {
            bail!("process event history was no longer retained")
        };
        let ProcessEventPageEvents::Full(page_events) = page.events else {
            unreachable!("full process event query returned a lite page");
        };
        events.extend(page_events);
        after_sequence = match page.more {
            ProcessEventPageMore::Complete => return Ok(events),
            ProcessEventPageMore::More { after_sequence } => after_sequence,
        };
    }
}

fn wake_batch_draft(wake: ProcessWakeDelivery) -> QueuedWorkBatchDraft {
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        lash::persistence::QueuedWorkPayload::process_wake(wake),
    )
    .with_merge_key(PROCESS_WAKE_MERGE_KEY)
    .with_source_key(process_wake_source_key(&process_id, sequence))
    .with_process_wake_source(process_id, sequence)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .context("usage: lash-e2e-process-operations-worker retarget|prepare|crash|recover|replacement-prepare|replacement-recover")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .context("connect process-operations worker to Postgres")?;

    match mode.as_str() {
        "retarget" => retarget(&storage).await,
        "prepare" => prepare(&storage).await,
        "crash" => crash_between_enqueue_and_mark(&storage).await,
        "recover" => recover_after_worker_restart(&storage).await,
        "replacement-prepare" | "replacement-recover" => replacement(&storage, &mode).await,
        other => bail!("unknown process-operations worker mode `{other}`"),
    }
}

async fn retarget(storage: &PostgresStorage) -> Result<()> {
    const RETARGET_PROCESS_ID: &str = "process-operations-retarget";
    const OLD_SESSION_ID: &str = "process-operations-retarget-old";
    const NEW_SESSION_ID: &str = "process-operations-retarget-new";
    let factory = storage.store();
    for session_id in [OLD_SESSION_ID, NEW_SESSION_ID] {
        factory
            .admit_session(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::parse(session_id.to_string())?,
                relation: SessionRelation::Root,
                config: SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                )
                .into(),
                head: SessionCreationHead::Config,
            })
            .await
            .with_context(|| format!("create retarget session `{session_id}`"))?;
    }
    let registry = registry(storage);
    let retarget_process = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: json!({"runbook": "process-operations"}),
                },
                ProcessProvenance::host(),
                Lifetime::Detached,
            )
            .with_admitted_identity(AdmittedProcessIdentity::pinned(ProcessIdentity::new(
                "runbook-retarget",
            )))
            .with_extra_event_types([ProcessEventType {
                name: EVENT_TYPE.to_string(),
                payload_schema: lash::triggers::JsonSchema::any(),
                semantics: ProcessEventSemanticsSpec {
                    wake: Some(ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some(SessionId::parse(OLD_SESSION_ID.to_string())?)),
        )
        .await
        .context("register retarget process")?
        .id;
    let old_wake = registry
        .append_event(
            &retarget_process,
            ProcessEventAppendRequest::new(EVENT_TYPE, json!({"wake_input": "old target"})),
        )
        .await
        .context("append old-target wake")?
        .wake_delivery
        .context("old-target wake outbox row was not created")?;
    registry
        .retarget_subscription(&retarget_process, Some(NEW_SESSION_ID))
        .await
        .context("retarget process subscription")?;
    let old_delivery = registry
        .list_wake_deliveries(None)
        .await
        .context("list retargeted sender rows")?
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == retarget_process
                && delivery.wake.sequence == old_wake.sequence
        })
        .context("old-target sender row is absent")?;
    anyhow::ensure!(
        old_delivery.state() == WakeDeliveryState::Discarded
            && old_delivery.disposition.discard_reason() == Some(WakeDiscardReason::Retargeted),
        "old pending delivery was not durably discarded as retargeted: {old_delivery:?}"
    );

    let new_wake = registry
        .append_event(
            &retarget_process,
            ProcessEventAppendRequest::new(EVENT_TYPE, json!({"wake_input": "new target"})),
        )
        .await
        .context("append new-target wake")?
        .wake_delivery
        .context("new-target wake outbox row was not created")?;
    anyhow::ensure!(
        new_wake.target_session_id == NEW_SESSION_ID,
        "new wake retained old target: {}",
        new_wake.target_session_id
    );
    let shift = WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::new(factory) as Arc<dyn DeploymentStore>,
        Arc::new(lash::runtime::NoSessionWork::new()),
        Arc::new(SystemClock),
        32,
    )
    .await
    .context("shift new-target wake")?;
    anyhow::ensure!(
        shift.enqueued == 1,
        "new-target shift was not singular: {shift:?}"
    );
    let old_batches = storage
        .store()
        .list_queued_work(&SessionId::from(OLD_SESSION_ID))
        .await
        .context("list old-target receiver rows")?;
    let new_batches = storage
        .store()
        .list_queued_work(&SessionId::from(NEW_SESSION_ID))
        .await
        .context("list new-target receiver rows")?;
    let audit_present = process_events(registry.as_ref(), &retarget_process)
        .await?
        .iter()
        .any(|event| event.event_type == "process.subscription_retargeted");
    anyhow::ensure!(
        old_batches.is_empty(),
        "old target received pending work after retarget"
    );
    anyhow::ensure!(
        new_batches.len() == 1,
        "new target did not receive exactly one wake"
    );
    anyhow::ensure!(audit_present, "retarget audit event is absent");
    println!(
        "{}",
        json!({
            "checkpoint": "retargeted",
            "process_id": RETARGET_PROCESS_ID,
            "old_delivery_state": old_delivery.state(),
            "old_discard_reason": old_delivery.disposition.discard_reason(),
            "audit_event": "process.subscription_retargeted",
            "old_target_turn_count": old_batches.len(),
            "new_target": new_wake.target_session_id,
            "new_sequence": new_wake.sequence,
            "new_target_turn_count": new_batches.len(),
        })
    );
    Ok(())
}

async fn prepare(storage: &PostgresStorage) -> Result<()> {
    storage
        .store()
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::parse(SESSION_ID.to_string())?,
            relation: SessionRelation::Root,
            config: SessionPolicy::new(lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(1024))
                .into(),
            head: SessionCreationHead::Config,
        })
        .await
        .context("create crash-recovery wake target")?;
    let registry = registry(storage);
    let process_id = registry
        .register_process(registration())
        .await
        .context("register crash-recovery process")?
        .id;
    let append = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                EVENT_TYPE,
                json!({"wake_input": "deliver exactly once after worker restart"}),
            )
            .with_replay_key("process-operations-crash-wake"),
        )
        .await
        .context("append crash-recovery wake")?;
    let wake = append
        .wake_delivery
        .context("wake outbox row was not created")?;
    println!(
        "{}",
        json!({
            "checkpoint": "prepared",
            "process_id": PROCESS_ID,
            "session_id": SESSION_ID,
            "delivery_id": wake.wake_id(),
            "sequence": wake.sequence,
        })
    );
    Ok(())
}

async fn crash_between_enqueue_and_mark(storage: &PostgresStorage) -> Result<()> {
    let registry = registry(storage);
    let process_id = crash_recovery_process(registry.as_ref()).await?;
    let claimed = registry
        .claim_pending_wake_deliveries(1)
        .await
        .context("claim crash-window wake")?;
    let delivery = claimed
        .into_iter()
        .find(|delivery| delivery.wake.process_id == process_id)
        .context("crash-window wake was not claimable")?;
    anyhow::ensure!(
        delivery.state() == WakeDeliveryState::Enqueuing,
        "claimed delivery was not enqueuing: {:?}",
        delivery.state()
    );

    let target = storage.store();
    let batch = target
        .enqueue_queued_work(wake_batch_draft(delivery.wake.clone()))
        .await
        .context("enqueue receiver row before crash")?;
    println!(
        "{}",
        json!({
            "checkpoint": "receiver_enqueued_sender_unmarked",
            "process_id": PROCESS_ID,
            "delivery_id": delivery.delivery_id(),
            "claim_token": delivery.claim_token().context("claimed delivery token")?,
            "batch_id": batch.batch_id,
            "enqueue_seq": batch.enqueue_seq,
        })
    );

    // The shell harness kills this container after observing the checkpoint.
    // Reaching either branch normally would invalidate the crash-window proof.
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn recover_after_worker_restart(storage: &PostgresStorage) -> Result<()> {
    let registry = registry(storage);
    let process_id = crash_recovery_process(registry.as_ref()).await?;
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    let report = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let report = WakeDeliveryDriver::drive_pending_once(
                Arc::clone(&registry),
                Arc::clone(&factory),
                Arc::new(lash::runtime::NoSessionWork::new()),
                Arc::new(SystemClock),
                32,
            )
            .await
            .context("recover stale crash-window delivery")?;
            if report.enqueued > 0 {
                return Ok::<_, anyhow::Error>(report);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("stale crash-window delivery was not recovered before timeout")??;

    let delivery = registry
        .list_wake_deliveries(None)
        .await
        .context("list recovered sender rows")?
        .into_iter()
        .find(|delivery| delivery.wake.process_id == process_id)
        .context("recovered sender row is absent")?;
    let batches = storage
        .store()
        .list_queued_work(&SessionId::from(SESSION_ID))
        .await
        .context("list recovered receiver rows")?
        .into_iter()
        .filter(|batch| {
            batch.source_key.as_deref()
                == Some(process_wake_source_key(&process_id, delivery.wake.sequence).as_str())
        })
        .collect::<Vec<_>>();

    anyhow::ensure!(
        delivery.state() == WakeDeliveryState::Enqueued,
        "recovered sender row did not settle enqueued: {:?}",
        delivery.state()
    );
    anyhow::ensure!(
        report.floor_absorbed == 1,
        "restart did not observe the durable receiver row: {report:?}"
    );
    anyhow::ensure!(
        delivery.attempts >= 2,
        "restart did not reclaim the original delivery: {delivery:?}"
    );
    anyhow::ensure!(
        batches.len() == 1,
        "restart produced {} receiver turns instead of exactly one",
        batches.len()
    );
    println!(
        "{}",
        json!({
            "checkpoint": "recovered_exactly_once",
            "process_id": PROCESS_ID,
            "delivery_id": delivery.delivery_id(),
            "sequence": delivery.wake.sequence,
            "attempts": delivery.attempts,
            "sender_state": delivery.state(),
            "floor_absorbed": report.floor_absorbed,
            "receiver_turn_count": batches.len(),
            "receiver_batch_id": batches[0].batch_id,
        })
    );
    Ok(())
}
