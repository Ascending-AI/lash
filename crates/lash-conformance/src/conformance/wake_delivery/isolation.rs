use super::*;
use lash_core::testing::ProcessRegistryFaults;
use pretty_assertions::assert_eq;

/// Backend-owned corruption and fresh-handle seams for wake recovery laws.
#[async_trait::async_trait]
pub trait WakeDeliveryIsolationBackend: Send + Sync {
    async fn corrupt_source(&self, process_id: &ProcessId);
    async fn reopen(
        &self,
    ) -> (
        Arc<dyn crate::DeploymentStore>,
        Arc<dyn crate::ProcessRegistry>,
    );
}

struct WakePage {
    target: crate::SessionStore,
    bad: crate::ProcessWakeDelivery,
    healthy: crate::ProcessWakeDelivery,
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
async fn wake_page(
    factory: &Arc<dyn crate::DeploymentStore>,
    registry: &Arc<dyn crate::ProcessRegistry>,
    clock: &TestClock,
) -> WakePage {
    let target_id = SessionId::from("wake-isolation-target");
    let target = factory
        .admit_view(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: target_id.clone(),
            relation: crate::SessionRelation::Root,
            config: crate::SessionPolicy::new(crate::TurnBudget::Unbounded).into(),
            head: crate::SessionCreationHead::CommittedByCreator,
        })
        .await
        .expect("create isolation target");
    let mut wakes = Vec::new();
    for label in ["bad", "healthy"] {
        let process = registry
            .register_process(
                process_registry::registration(label)
                    .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                    .with_wake_session_id(Some(target_id.clone())),
            )
            .await
            .expect("register isolation source");
        wakes.push(
            registry
                .append_event(
                    &process.id,
                    crate::ProcessEventAppendRequest::new(
                        "producer.wake",
                        serde_json::json!({"wake_input": label}),
                    ),
                )
                .await
                .expect("append isolation wake")
                .wake_delivery
                .expect("durable wake"),
        );
        // Due time, rather than a randomly minted process id, fixes the page order.
        clock.advance(1);
    }
    WakePage {
        target,
        bad: wakes.remove(0),
        healthy: wakes.remove(0),
    }
}

async fn drive(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
) -> Result<crate::WakeDeliveryDriveReport, crate::PluginError> {
    crate::WakeDeliveryDriver::drive_pending_once(
        registry,
        factory,
        Arc::new(crate::NoSessionWork::new()),
        clock,
        2,
    )
    .await
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
async fn assert_healthy_enqueued(page: &WakePage, registry: &Arc<dyn crate::ProcessRegistry>) {
    let queue = page
        .target
        .list_queued_work()
        .await
        .expect("read receiver queue");
    assert_eq!(
        queue.len(),
        1,
        "exactly the healthy wake must reach the receiver"
    );
    assert_eq!(
        queue[0].source_key.as_deref(),
        Some(
            crate::process_wake_source_key(&page.healthy.process_id, page.healthy.sequence)
                .as_str()
        )
    );
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect claimed siblings");
    let healthy = rows
        .iter()
        .find(|row| row.delivery_id == page.healthy.wake_id)
        .expect("healthy delivery");
    assert_eq!(healthy.state(), crate::WakeDeliveryState::Enqueued);
    assert!(
        rows.iter()
            .all(|row| row.state() != crate::WakeDeliveryState::Enqueuing),
        "no sibling may remain trapped in the claimed page"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn bad_wake_source_does_not_strand_claimed_siblings(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    let before = registry
        .get_process(&page.healthy.process_id)
        .await
        .expect("read healthy source");
    let tail = registry
        .append_event(
            &page.bad.process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "blocked tail"}),
            ),
        )
        .await
        .expect("append later wake in the bad source's ordering group")
        .wake_delivery
        .expect("tail wake");
    backend.corrupt_source(&page.bad.process_id).await;
    assert!(
        registry.get_process(&page.bad.process_id).await.is_err(),
        "the first source must fail permanently"
    );
    let report = drive(factory, Arc::clone(&registry), clock)
        .await
        .expect("a bad source must not abort the claimed page");
    assert_eq!(report.inspected, 2);
    assert_eq!(report.enqueued, 1);
    assert_healthy_enqueued(&page, &registry).await;
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect bad source disposition");
    let bad = rows
        .iter()
        .find(|row| row.delivery_id == page.bad.wake_id)
        .expect("bad delivery");
    assert_eq!(
        bad.state(),
        crate::WakeDeliveryState::Discarded,
        "permanent source failure must have a durable terminal disposition"
    );
    assert_eq!(
        bad.disposition
            .discard_reason()
            .map(crate::WakeDiscardReason::as_str),
        Some("source_unreadable")
    );
    let tail_row = rows
        .iter()
        .find(|row| row.delivery_id == tail.wake_id)
        .expect("blocked tail delivery");
    assert_eq!(tail_row.state(), crate::WakeDeliveryState::Pending);
    assert_eq!(
        tail_row.attempts, 0,
        "a failed group head must preserve per-process ordering"
    );
    let durable_report = registry
        .wake_delivery_report()
        .await
        .expect("inspect blocked source group");
    assert_eq!(durable_report.blocked_groups.len(), 1);
    assert_eq!(
        durable_report.blocked_groups[0].blocking_delivery_id,
        page.bad.wake_id
    );
    assert_eq!(
        durable_report.blocked_groups[0].redrive_delivery_id,
        page.bad.wake_id
    );
    assert_eq!(
        serde_json::to_value(&durable_report).expect("serialize durable report")["source_unreadable"],
        1
    );
    assert_eq!(
        registry
            .get_process(&page.healthy.process_id)
            .await
            .expect("read healthy source after delivery"),
        before,
        "delivery must not fabricate a process outcome"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn expired_wakes_settle_without_reading_bad_sources(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    backend.corrupt_source(&page.bad.process_id).await;
    let expiry = registry
        .list_wake_deliveries(None)
        .await
        .expect("read expiry")
        .iter()
        .map(|row| row.expires_at_ms)
        .max()
        .expect("wake expiry");
    clock.set(expiry);
    let faults = Arc::new(ProcessRegistryFaults::new(Arc::clone(&registry)));
    faults.set_process_read_error(Some(crate::PluginError::Session(
        "source unavailable".into(),
    )));
    let report = drive(factory, faults.clone(), clock)
        .await
        .expect("expiry must not depend on source reads");
    assert_eq!(report.inspected, 2);
    assert_eq!(report.discarded_expired, 2);
    assert_eq!(faults.process_point_reads(), 0);
    assert!(
        page.target
            .list_queued_work()
            .await
            .expect("expired queue")
            .is_empty()
    );
    for row in registry
        .list_wake_deliveries(None)
        .await
        .expect("expired dispositions")
    {
        assert_eq!(
            row.disposition.discard_reason(),
            Some(crate::WakeDiscardReason::Expired)
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn transient_wake_source_retries_release_claims_and_keep_expiry(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    _backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    let faults = Arc::new(ProcessRegistryFaults::new(Arc::clone(&registry)));
    faults.set_process_read_error_after(
        0,
        crate::PluginError::Session("connection temporarily closed".into()),
    );
    let report = drive(Arc::clone(&factory), faults.clone(), Arc::clone(&clock))
        .await
        .expect("transient source error must release only its claim");
    assert_eq!(report.enqueued, 1);
    assert_eq!(report.retryable_failures, 1);
    assert_healthy_enqueued(&page, &registry).await;
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect deferred source");
    let bad = rows
        .iter()
        .find(|row| row.delivery_id == page.bad.wake_id)
        .expect("deferred delivery");
    assert_eq!(bad.state(), crate::WakeDeliveryState::Pending);
    assert!(
        bad.next_attempt_at_ms > crate::ClockWallTime::timestamp_ms(clock.as_ref()),
        "transient failures need backoff"
    );
    let expiry = bad.expires_at_ms;
    clock.set(expiry - 1);
    faults.set_process_read_error_after(0, crate::PluginError::Session("still unavailable".into()));
    let retry = drive(Arc::clone(&factory), faults.clone(), Arc::clone(&clock))
        .await
        .expect("retry near expiry");
    assert_eq!(retry.retryable_failures, 1);
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect expiry bound");
    let bad = rows
        .iter()
        .find(|row| row.delivery_id == page.bad.wake_id)
        .expect("bounded retry");
    assert_eq!(
        bad.next_attempt_at_ms, expiry,
        "backoff must not postpone expiry"
    );
    assert_eq!(bad.expires_at_ms, expiry);
    clock.set(expiry);
    faults.set_process_read_error(Some(crate::PluginError::Session(
        "still unavailable".into(),
    )));
    let expired = drive(factory, faults, clock)
        .await
        .expect("expire deferred source");
    assert_eq!(expired.discarded_expired, 1);
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn wake_defer_failure_does_not_strand_claimed_siblings(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    _backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    let faults = Arc::new(ProcessRegistryFaults::new(Arc::clone(&registry)));
    faults.set_process_read_error_after(
        0,
        crate::PluginError::Session("source read unavailable".into()),
    );
    faults.set_wake_defer_error(Some(crate::PluginError::Session(
        "defer write unavailable".into(),
    )));
    let report = drive(Arc::clone(&factory), faults, Arc::clone(&clock))
        .await
        .expect("a failed retry write must not abort the page");
    assert_eq!(report.inspected, 2);
    assert_eq!(report.enqueued, 1);
    assert_eq!(report.retryable_failures, 1);
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect failed retry");
    let bad = rows
        .iter()
        .find(|row| row.delivery_id == page.bad.wake_id)
        .expect("failed retry row");
    assert_eq!(
        bad.state(),
        crate::WakeDeliveryState::Enqueuing,
        "only the delivery whose write failed awaits claim lapse"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.state() == crate::WakeDeliveryState::Enqueuing)
            .count(),
        1
    );
    assert_eq!(
        page.target
            .list_queued_work()
            .await
            .expect("healthy receiver queue")
            .len(),
        1
    );
    clock.set(bad.next_attempt_at_ms);
    assert_eq!(
        drive(factory, Arc::clone(&registry), clock)
            .await
            .expect("recover failed retry after claim lapse")
            .enqueued,
        1
    );
    assert_eq!(
        registry
            .wake_delivery_report()
            .await
            .expect("recovered report")
            .enqueued,
        2
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn bad_wake_source_page_recovers_after_restart(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    let claims = registry
        .claim_pending_wake_deliveries(2)
        .await
        .expect("claim page before restart");
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].wake.process_id, page.bad.process_id);
    backend.corrupt_source(&page.bad.process_id).await;
    drop(factory);
    drop(registry);
    let (factory, registry) = backend.reopen().await;
    clock.set(
        claims
            .iter()
            .map(|row| row.next_attempt_at_ms)
            .max()
            .expect("claim lapse"),
    );
    let report = drive(factory, Arc::clone(&registry), clock)
        .await
        .expect("restart must recover the full claimed page");
    assert_eq!(report.enqueued, 1);
    assert_healthy_enqueued(&page, &registry).await;
    for stale in claims {
        assert!(matches!(
            registry
                .mark_wake_enqueued(&stale.delivery_id, stale.claim_token().expect("old token"))
                .await
                .expect("old owner is fenced"),
            crate::WakeDeliveryClaimOutcome::ClaimLost { .. }
        ));
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn lost_bad_source_claim_does_not_settle_the_new_owner(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    backend: Arc<dyn WakeDeliveryIsolationBackend>,
) {
    let page = wake_page(&factory, &registry, &clock).await;
    let stale = registry
        .claim_pending_wake_deliveries(2)
        .await
        .expect("first page owner");
    clock.set(
        stale
            .iter()
            .map(|row| row.next_attempt_at_ms)
            .max()
            .expect("first claim lapse"),
    );
    let current = registry
        .claim_pending_wake_deliveries(2)
        .await
        .expect("replacement page owner");
    assert_eq!(current.len(), 2);
    backend.corrupt_source(&page.bad.process_id).await;
    let faults = Arc::new(ProcessRegistryFaults::new(Arc::clone(&registry)));
    for row in &stale {
        faults.inject_claimed_wake(row.clone());
    }
    let report = drive(factory, faults, clock)
        .await
        .expect("lost source claim is a benign fenced settlement");
    assert_eq!(report.inspected, 2);
    assert_eq!(report.enqueued, 0);
    let rows = registry
        .list_wake_deliveries(None)
        .await
        .expect("inspect replacement claims");
    for claim in &current {
        let row = rows
            .iter()
            .find(|row| row.delivery_id == claim.delivery_id)
            .expect("replacement row");
        assert_eq!(
            row.disposition, claim.disposition,
            "stale driver must not mutate the new ownership fence"
        );
    }
    let bad = current
        .iter()
        .find(|row| row.delivery_id == page.bad.wake_id)
        .expect("new bad source owner");
    let healthy = current
        .iter()
        .find(|row| row.delivery_id == page.healthy.wake_id)
        .expect("new healthy source owner");
    assert_eq!(
        registry
            .discard_wake_delivery(
                &bad.delivery_id,
                bad.claim_token().expect("new bad token"),
                crate::WakeDiscardReason::Expired
            )
            .await
            .expect("new owner settles bad source"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    assert_eq!(
        registry
            .mark_wake_enqueued(
                &healthy.delivery_id,
                healthy.claim_token().expect("new healthy token")
            )
            .await
            .expect("new owner settles healthy source"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    assert_eq!(
        page.target
            .list_queued_work()
            .await
            .expect("receiver dedupes lost claims")
            .len(),
        1
    );
}
