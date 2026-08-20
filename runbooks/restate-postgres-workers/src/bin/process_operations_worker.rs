use anyhow::{Context, Result, bail};
use lash::persistence::{
    DeliveryPolicy, PROCESS_WAKE_MERGE_KEY, QueuedWorkBatchDraft, QueuedWorkPayload,
    QueuedWorkStore as _,
};
use lash::process::{WakeDeliveryDriver, process_wake_source_key};
use lash_core::{
    ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessEventType, ProcessIdentity,
    ProcessInput, ProcessProvenance, ProcessRegistration, ProcessValueSelector,
    ProcessWakeDelivery, ProcessWakeSpec, RecoveryContract, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory as _, WakeDeliveryConfig, WakeDeliveryState,
    WakeDiscardReason,
};
use lash_postgres_store::PostgresStorage;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const PROCESS_ID: &str = "process-operations-crash-recovery";
const SESSION_ID: &str = "process-operations-crash-target";
const EVENT_TYPE: &str = "runbook.wake";

fn registry(storage: &PostgresStorage) -> Arc<dyn lash_core::ProcessRegistry> {
    Arc::new(
        storage.process_registry_with_wake_delivery_config(
            WakeDeliveryConfig::new(60_000)
                .expect("valid runbook wake expiry")
                .with_enqueuing_stale_after_ms(1)
                .expect("valid runbook stale-claim age"),
        ),
    )
}

fn registration() -> ProcessRegistration {
    ProcessRegistration::new(
        PROCESS_ID,
        ProcessInput::External {
            metadata: json!({"runbook": "process-operations"}),
        },
        RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
    )
    .with_identity(
        ProcessIdentity::new("runbook")
            .with_label(Some(PROCESS_ID))
            .with_definition(Some(json!({"scenario": "worker-crash-recovery"}))),
    )
    .with_extra_event_types([ProcessEventType {
        name: EVENT_TYPE.to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            wake: Some(ProcessWakeSpec {
                when: None,
                input: ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }])
    .with_wake_session_id(Some(SESSION_ID.to_string()))
}

fn wake_batch_draft(wake: ProcessWakeDelivery) -> QueuedWorkBatchDraft {
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        DeliveryPolicy::EarliestSafeBoundary,
        vec![QueuedWorkPayload::process_wake(wake)],
    )
    .with_merge_key(PROCESS_WAKE_MERGE_KEY)
    .with_source_key(process_wake_source_key(&process_id, sequence))
    .with_process_wake_source(process_id, sequence)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .context("usage: lash-e2e-process-operations-worker retarget|prepare|crash|recover")?;
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .context("connect process-operations worker to Postgres")?;

    match mode.as_str() {
        "retarget" => retarget(&storage).await,
        "prepare" => prepare(&storage).await,
        "crash" => crash_between_enqueue_and_mark(&storage).await,
        "recover" => recover_after_worker_restart(&storage).await,
        other => bail!("unknown process-operations worker mode `{other}`"),
    }
}

async fn retarget(storage: &PostgresStorage) -> Result<()> {
    const RETARGET_PROCESS_ID: &str = "process-operations-retarget";
    const OLD_SESSION_ID: &str = "process-operations-retarget-old";
    const NEW_SESSION_ID: &str = "process-operations-retarget-new";
    let factory = storage.session_store_factory_with_shared_process_registry();
    for session_id in [OLD_SESSION_ID, NEW_SESSION_ID] {
        factory
            .create_store(&SessionStoreCreateRequest {
                session_id: session_id.to_string(),
                relation: SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .with_context(|| format!("create retarget session `{session_id}`"))?;
    }
    let registry = registry(storage);
    registry
        .register_process(
            ProcessRegistration::new(
                RETARGET_PROCESS_ID,
                ProcessInput::External {
                    metadata: json!({"runbook": "process-operations"}),
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            )
            .with_identity(ProcessIdentity::new("runbook-retarget"))
            .with_extra_event_types([ProcessEventType {
                name: EVENT_TYPE.to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: ProcessEventSemanticsSpec {
                    wake: Some(ProcessWakeSpec {
                        when: None,
                        input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some(OLD_SESSION_ID.to_string())),
        )
        .await
        .context("register retarget process")?;
    let old_wake = registry
        .append_event(
            RETARGET_PROCESS_ID,
            ProcessEventAppendRequest::new(EVENT_TYPE, json!({"wake_input": "old target"})),
        )
        .await
        .context("append old-target wake")?
        .wake_delivery
        .context("old-target wake outbox row was not created")?;
    registry
        .retarget_subscription(RETARGET_PROCESS_ID, Some(NEW_SESSION_ID))
        .await
        .context("retarget process subscription")?;
    let old_delivery = registry
        .list_wake_deliveries(None)
        .await
        .context("list retargeted sender rows")?
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == RETARGET_PROCESS_ID
                && delivery.wake.sequence == old_wake.sequence
        })
        .context("old-target sender row is absent")?;
    anyhow::ensure!(
        old_delivery.state == WakeDeliveryState::Discarded
            && old_delivery.discard_reason == Some(WakeDiscardReason::Retargeted),
        "old pending delivery was not durably discarded as retargeted: {old_delivery:?}"
    );

    let new_wake = registry
        .append_event(
            RETARGET_PROCESS_ID,
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
    let drive = WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::new(factory) as Arc<dyn lash_core::SessionStoreFactory>,
        None,
        Arc::new(lash_core::facade_support::SystemClock),
        32,
    )
    .await
    .context("drive new-target wake")?;
    anyhow::ensure!(
        drive.enqueued == 1,
        "new-target drive was not singular: {drive:?}"
    );
    let old_batches = storage
        .session_store(OLD_SESSION_ID)
        .list_queued_work(OLD_SESSION_ID)
        .await
        .context("list old-target receiver rows")?;
    let new_batches = storage
        .session_store(NEW_SESSION_ID)
        .list_queued_work(NEW_SESSION_ID)
        .await
        .context("list new-target receiver rows")?;
    let audit_present = registry
        .events_after(RETARGET_PROCESS_ID, 0)
        .await
        .context("read retarget audit events")?
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
            "old_delivery_state": old_delivery.state,
            "old_discard_reason": old_delivery.discard_reason,
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
        .session_store_factory_with_shared_process_registry()
        .create_store(&SessionStoreCreateRequest {
            session_id: SESSION_ID.to_string(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .context("create crash-recovery wake target")?;
    let registry = registry(storage);
    registry
        .register_process(registration())
        .await
        .context("register crash-recovery process")?;
    let append = registry
        .append_event(
            PROCESS_ID,
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
            "delivery_id": wake.wake_id,
            "sequence": wake.sequence,
        })
    );
    Ok(())
}

async fn crash_between_enqueue_and_mark(storage: &PostgresStorage) -> Result<()> {
    let registry = registry(storage);
    let claimed = registry
        .claim_pending_wake_deliveries(1)
        .await
        .context("claim crash-window wake")?;
    let delivery = claimed
        .into_iter()
        .find(|delivery| delivery.wake.process_id == PROCESS_ID)
        .context("crash-window wake was not claimable")?;
    anyhow::ensure!(
        delivery.state == WakeDeliveryState::Enqueuing,
        "claimed delivery was not enqueuing: {:?}",
        delivery.state
    );

    let target = storage.session_store(SESSION_ID);
    let batch = target
        .enqueue_queued_work(wake_batch_draft(delivery.wake.clone()))
        .await
        .context("enqueue receiver row before crash")?;
    println!(
        "{}",
        json!({
            "checkpoint": "receiver_enqueued_sender_unmarked",
            "process_id": PROCESS_ID,
            "delivery_id": delivery.delivery_id,
            "claim_token": delivery.claim_token,
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
    let factory = Arc::new(storage.session_store_factory_with_shared_process_registry())
        as Arc<dyn lash_core::SessionStoreFactory>;
    let report = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let report = WakeDeliveryDriver::drive_pending_once(
                Arc::clone(&registry),
                Arc::clone(&factory),
                None,
                Arc::new(lash_core::facade_support::SystemClock),
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
        .find(|delivery| delivery.wake.process_id == PROCESS_ID)
        .context("recovered sender row is absent")?;
    let batches = storage
        .session_store(SESSION_ID)
        .list_queued_work(SESSION_ID)
        .await
        .context("list recovered receiver rows")?
        .into_iter()
        .filter(|batch| {
            batch.source_key.as_deref()
                == Some(process_wake_source_key(PROCESS_ID, delivery.wake.sequence).as_str())
        })
        .collect::<Vec<_>>();

    anyhow::ensure!(
        delivery.state == WakeDeliveryState::Enqueued,
        "recovered sender row did not settle enqueued: {:?}",
        delivery.state
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
            "delivery_id": delivery.delivery_id,
            "sequence": delivery.wake.sequence,
            "attempts": delivery.attempts,
            "sender_state": delivery.state,
            "floor_absorbed": report.floor_absorbed,
            "receiver_turn_count": batches.len(),
            "receiver_batch_id": batches[0].batch_id,
        })
    );
    Ok(())
}
