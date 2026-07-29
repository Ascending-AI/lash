use super::*;

async fn append_wake(
    registry: &Arc<dyn crate::ProcessRegistry>,
    process_id: &str,
    target_session_id: &str,
) -> crate::WakeDelivery {
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")]),
        )
        .await
        .expect("register wake producer");
    let append = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": format!("wake from {process_id}")}),
            )
            .with_wake_target_scope(crate::SessionScope::new(target_session_id)),
        )
        .await
        .expect("append wake event");
    let wake = append.wake_delivery.expect("wake delivery");
    registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("list pending wake deliveries")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == process_id && delivery.wake.sequence == wake.sequence
        })
        .expect("appended outbox row")
}

async fn drive_once(
    registry: Arc<dyn crate::ProcessRegistry>,
    factory: Arc<dyn crate::SessionStoreFactory>,
) -> crate::WakeDeliveryDriveReport {
    crate::WakeDeliveryDriver::drive_pending_once(
        registry,
        factory,
        None,
        Arc::new(crate::SystemClock),
        32,
    )
    .await
    .expect("drive pending wakes")
}

/// Cross-backend process-wake crash matrix.
///
/// The supplied process registry and session factory must share their normal
/// backend durability domain, but need not share a database transaction: the
/// sender outbox and receiver evidence deliberately bridge that crash boundary.
pub async fn wake_delivery_crash_matrix(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let target_request = crate::SessionStoreCreateRequest {
        session_id: "wake-crash-target".to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy {
            model: crate::ModelSpec::from_token_limits(
                "wake-crash-model",
                Default::default(),
                200_000,
                None,
            )
            .expect("valid crash-matrix model"),
            provider_id: "conformance-provider".to_string(),
            session_id: Some("wake-crash-target".to_string()),
            autonomous: false,
            max_turns: None,
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        },
    };
    let target = factory
        .create_store(&target_request)
        .await
        .expect("create wake target");

    // Crash after process append, before enqueue: startup scan finds the row.
    let append_only = append_wake(&registry, "wake-append-only", &target_request.session_id).await;
    let folded_before_delivery = registry
        .try_get_process("wake-append-only")
        .await
        .expect("read process before delivery transition")
        .expect("wake process exists");
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue before startup scan")
            .is_empty()
    );
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.enqueued, 1);
    assert_eq!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Enqueued))
            .await
            .expect("enqueued outbox rows")
            .iter()
            .filter(|delivery| delivery.delivery_id == append_only.delivery_id)
            .count(),
        1
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .try_get_process("wake-append-only")
                .await
                .expect("read process after delivery transition")
                .expect("wake process remains")
        )
        .expect("serialize record after delivery"),
        serde_json::to_value(folded_before_delivery).expect("serialize record before delivery"),
        "delivery writes must never change the folded process record"
    );
    assert!(
        registry
            .redrive_wake_delivery(&append_only.delivery_id)
            .await
            .is_err(),
        "redrive must reject an already-enqueued delivery"
    );

    // Terminal and wake semantics are independent: a producer-declared event
    // carrying both must append its delivery row in the same transaction.
    let terminal_wake_type = crate::ProcessEventType {
        name: "producer.terminal_wake".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec {
            terminal: Some(crate::ProcessTerminalSpec {
                state: crate::ProcessTerminalState::Completed,
                await_output: None,
            }),
            wake: Some(crate::ProcessWakeSpec {
                when: None,
                input: crate::ProcessValueSelector::Pointer("/wake_input".to_string()),
            }),
        },
    };
    registry
        .register_process(
            process_registry::registration("wake-terminal-event")
                .with_extra_event_types([terminal_wake_type]),
        )
        .await
        .expect("register terminal wake producer");
    let terminal_append = registry
        .append_event(
            "wake-terminal-event",
            crate::ProcessEventAppendRequest::new(
                "producer.terminal_wake",
                serde_json::json!({"wake_input": "terminal wake"}),
            )
            .with_replay_key("terminal-wake")
            .with_wake_target_scope(crate::SessionScope::new(&target_request.session_id)),
        )
        .await
        .expect("append terminal wake event");
    let terminal_delivery = terminal_append
        .wake_delivery
        .expect("terminal event materializes wake");
    assert!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
            .await
            .expect("list terminal wake outbox")
            .iter()
            .any(|delivery| {
                delivery.wake.process_id == terminal_delivery.process_id
                    && delivery.wake.sequence == terminal_delivery.sequence
            }),
        "terminal wake event must commit its outbox row"
    );

    registry
        .register_process(
            process_registry::registration("wake-ordered")
                .with_extra_event_types([process_registry::wake_event_type("producer.ordered")]),
        )
        .await
        .expect("register ordered wake producer");
    for value in ["first", "second"] {
        registry
            .append_event(
                "wake-ordered",
                crate::ProcessEventAppendRequest::new(
                    "producer.ordered",
                    serde_json::json!({"wake_input": value}),
                )
                .with_wake_target_scope(crate::SessionScope::new(&target_request.session_id)),
            )
            .await
            .expect("append ordered wake");
    }
    let ordered = registry
        .pending_wake_deliveries(64)
        .await
        .expect("scan ordered wake deliveries")
        .into_iter()
        .filter(|delivery| delivery.wake.process_id == "wake-ordered")
        .map(|delivery| delivery.wake.sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        ordered,
        vec![1],
        "later same-target/process sequence must remain blocked behind its pending predecessor"
    );

    // Crash after enqueue, before sender mark: replay returns the live queue row.
    let before_mark = append_wake(&registry, "wake-before-mark", &target_request.session_id).await;
    let before_mark_batch = target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("simulate enqueue before crash");
    let replayed_live = target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("replay live enqueue before sender mark");
    assert_eq!(replayed_live.batch_id, before_mark_batch.batch_id);
    let queued = target
        .list_queued_work(&target_request.session_id)
        .await
        .expect("list live queued wakes");
    assert_eq!(
        queued
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&before_mark.source_key()))
            .count(),
        1
    );

    // Settle the queued wake and then redrive the sender row. Receiver evidence
    // must absorb the late redelivery without recreating work.
    let owner = crate::LeaseOwnerIdentity::opaque("wake-crash-owner", "wake-crash-incarnation");
    let lease = match target
        .try_claim_session_execution_lease(&target_request.session_id, &owner, 60_000)
        .await
        .expect("claim target session")
    {
        crate::SessionExecutionLeaseClaimOutcome::Acquired(lease) => lease,
        crate::SessionExecutionLeaseClaimOutcome::Busy { .. } => {
            panic!("fresh crash-matrix target lease must be available")
        }
    };
    let claim = target
        .claim_ready_queued_work_by_batch_ids(
            &target_request.session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&before_mark_batch.batch_id),
        )
        .await
        .expect("claim queued wakes")
        .expect("queued wake claim");
    let state = crate::RuntimeSessionState {
        session_id: target_request.session_id.clone(),
        ..crate::RuntimeSessionState::default()
    };
    target
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion())
                .releasing_session_execution_lease(lease.completion()),
        )
        .await
        .expect("settle queued wakes and evidence atomically");
    let consumed_replay = target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("receiver evidence absorbs redelivery while sender remains pending");
    assert_ne!(consumed_replay.batch_id, before_mark_batch.batch_id);
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue while consumed evidence is retained")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key()))
    );
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after consumed redelivery")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key())),
        "late sender redelivery must resolve against receiver evidence"
    );
    let reconciled = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Enqueued))
        .await
        .expect("list reconciled sender rows")
        .into_iter()
        .find(|delivery| delivery.delivery_id == before_mark.delivery_id)
        .expect("late redelivery sender row becomes terminal");
    assert!(
        !reconciled.evidence_cleanup_pending,
        "terminal sender row must reconcile exact receiver evidence"
    );
    target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("exact evidence was pruned after sender terminality");
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after terminal evidence pruning")
            .iter()
            .any(|batch| batch.source_key.as_deref() == Some(&before_mark.source_key())),
        "evidence cleanup must occur only after the sender row is terminal"
    );

    // Competing drivers may both inspect a pending row, but queue and sender
    // transitions remain idempotent.
    let concurrent = append_wake(
        &registry,
        "wake-concurrent-drivers",
        &target_request.session_id,
    )
    .await;
    let (left, right) = tokio::join!(
        drive_once(Arc::clone(&registry), Arc::clone(&factory)),
        drive_once(Arc::clone(&registry), Arc::clone(&factory))
    );
    assert!(
        left.enqueued + right.enqueued >= 1,
        "at least one competing driver must enqueue the pending wake"
    );
    assert_eq!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after concurrent drivers")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&concurrent.source_key()))
            .count(),
        1
    );

    // A never-created receiver remains pending until it appears.
    let future_target_id = "future-wake-target";
    let future = append_wake(&registry, "wake-future-target", future_target_id).await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_target_gone, 0);
    assert!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
            .await
            .expect("pending future-target deliveries")
            .iter()
            .any(|delivery| delivery.delivery_id == future.delivery_id)
    );
    factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: future_target_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: target_request.policy.clone(),
        })
        .await
        .expect("create future wake target");
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.enqueued, 1);

    // A permanently tombstoned receiver is a typed terminal outcome; explicit
    // redrive is the only operation that returns it to the pending lane.
    let gone_target_id = "deleted-wake-target";
    factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: gone_target_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: target_request.policy.clone(),
        })
        .await
        .expect("create target to tombstone");
    factory
        .delete_session(gone_target_id)
        .await
        .expect("tombstone wake target");
    let gone = append_wake(&registry, "wake-target-gone", gone_target_id).await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_target_gone, 1);
    let discarded = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Discarded))
        .await
        .expect("discarded deliveries")
        .into_iter()
        .find(|delivery| delivery.delivery_id == gone.delivery_id)
        .expect("target-gone delivery");
    assert_eq!(
        discarded.discard_reason,
        Some(crate::WakeDiscardReason::TargetGone)
    );
    registry
        .redrive_wake_delivery(&gone.delivery_id)
        .await
        .expect("explicitly redrive target-gone row");
    assert!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
            .await
            .expect("pending after redrive")
            .iter()
            .any(|delivery| delivery.delivery_id == gone.delivery_id)
    );
    registry
        .discard_wake_delivery(&gone.delivery_id, crate::WakeDiscardReason::TargetGone)
        .await
        .expect("return redriven missing target to its typed discard");

    // Expiry is a typed terminal outcome. Redrive is explicit, refreshes the
    // expiry horizon, and lets the same row deliver normally.
    let expiry = append_wake(&registry, "wake-expiry", &target_request.session_id).await;
    let wait_ms = expiry
        .expires_at_ms
        .saturating_sub(crate::current_epoch_ms())
        .saturating_add(5);
    assert!(
        wait_ms <= 5_000,
        "crash-matrix registries must use a short test expiry, got {wait_ms}ms"
    );
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_expired, 1);
    registry
        .redrive_wake_delivery(&expiry.delivery_id)
        .await
        .expect("redrive expired delivery");
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.enqueued, 1);
    assert_eq!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after expired redrive")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(&expiry.source_key()))
            .count(),
        1
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::SessionStoreFactory as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct WakeTestClock {
        timestamp_ms: AtomicU64,
        base: Instant,
    }

    impl WakeTestClock {
        fn new(timestamp_ms: u64) -> Self {
            Self {
                timestamp_ms: AtomicU64::new(timestamp_ms),
                base: Instant::now(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::Clock for WakeTestClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.timestamp_ms())
        }

        fn timestamp_ms(&self) -> u64 {
            self.timestamp_ms.load(Ordering::SeqCst)
        }

        fn timestamp_rfc3339(&self) -> String {
            self.timestamp_datetime().to_rfc3339()
        }

        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp_millis(self.timestamp_ms() as i64)
                .expect("test datetime")
        }

        async fn sleep(&self, _duration: Duration) {}

        async fn sleep_until(&self, _deadline: Instant) {}
    }

    #[tokio::test]
    async fn in_memory_wake_delivery_crash_matrix() {
        let config = crate::WakeDeliveryConfig::new(250).expect("valid short test retention");
        let registry =
            Arc::new(crate::TestLocalProcessRegistry::default().with_wake_delivery_config(config))
                as Arc<dyn crate::ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::new())
            as Arc<dyn crate::SessionStoreFactory>;
        Box::pin(wake_delivery_crash_matrix(factory, registry)).await;
    }

    #[tokio::test]
    async fn injected_clock_wake_is_delivered_instead_of_expired() {
        let clock = Arc::new(WakeTestClock::new(0));
        let config = crate::WakeDeliveryConfig::new(250).expect("valid short test expiry");
        let registry = Arc::new(
            crate::TestLocalProcessRegistry::default()
                .with_clock(clock.clone())
                .with_wake_delivery_config(config),
        ) as Arc<dyn crate::ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::with_clock(
            clock.clone(),
        )) as Arc<dyn crate::SessionStoreFactory>;
        let request = crate::SessionStoreCreateRequest {
            session_id: "sim-clock-wake-target".to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        };
        let target = factory
            .create_store(&request)
            .await
            .expect("create sim-clock target");
        let delivery = append_wake(&registry, "sim-clock-wake", &request.session_id).await;
        let report = crate::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&registry),
            Arc::clone(&factory),
            None,
            clock,
            32,
        )
        .await
        .expect("drive with injected clock");
        assert_eq!(report.enqueued, 1);
        assert_eq!(report.discarded_expired, 0);
        assert!(
            target
                .list_queued_work(&request.session_id)
                .await
                .expect("list sim-clock queue")
                .iter()
                .any(|batch| batch.source_key.as_deref() == Some(&delivery.source_key()))
        );
    }

    #[tokio::test]
    async fn wake_delivery_driver_shutdown_releases_background_store_handles() {
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let factory = Arc::new(crate::InMemorySessionStoreFactory::new());
        let registry_weak = Arc::downgrade(&registry);
        let factory_weak = Arc::downgrade(&factory);
        let driver = crate::WakeDeliveryDriver::new(
            registry.clone(),
            factory.clone(),
            None,
            Arc::new(crate::SystemClock),
        );
        drop(registry);
        drop(factory);
        tokio::time::timeout(Duration::from_secs(1), driver.shutdown())
            .await
            .expect("wake driver task must exit on shutdown");
        drop(driver);
        assert!(registry_weak.upgrade().is_none());
        assert!(factory_weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn discard_winning_after_enqueue_compensates_the_live_batch() {
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let registry_dyn = registry.clone() as Arc<dyn crate::ProcessRegistry>;
        let factory = Arc::new(crate::InMemorySessionStoreFactory::new());
        let factory_dyn = factory.clone() as Arc<dyn crate::SessionStoreFactory>;
        let request = crate::SessionStoreCreateRequest {
            session_id: "wake-discard-race-target".to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        };
        let target = factory
            .create_store(&request)
            .await
            .expect("create discard-race target");
        let delivery = append_wake(&registry_dyn, "wake-discard-race", &request.session_id).await;
        let pause = registry.pause_next_wake_mark();
        let drive = crate::task::spawn(crate::WakeDeliveryDriver::drive_pending_once(
            registry_dyn.clone(),
            factory_dyn,
            None,
            Arc::new(crate::SystemClock),
            32,
        ));
        pause.wait_until_validated().await;
        registry
            .discard_wake_delivery(&delivery.delivery_id, crate::WakeDiscardReason::TargetGone)
            .await
            .expect("discard wins sender transition");
        pause.resume();
        drive
            .await
            .expect("join discard-race drive")
            .expect("compensate stale enqueue");
        assert!(
            target
                .list_queued_work(&request.session_id)
                .await
                .expect("list queue after discard compensation")
                .iter()
                .all(|batch| batch.source_key.as_deref() != Some(&delivery.source_key())),
            "a discarded delivery must never leave a live queued batch"
        );
    }

    #[tokio::test]
    async fn in_memory_append_hides_outbox_until_event_and_fold_commit() {
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        registry
            .register_process(
                process_registry::registration("wake-atomic-append")
                    .with_extra_event_types([process_registry::wake_event_type("producer.atomic")]),
            )
            .await
            .expect("register atomic append process");
        let pause = registry.pause_next_append_after_outbox();
        let append_registry = Arc::clone(&registry);
        let append = crate::task::spawn(async move {
            append_registry
                .append_event(
                    "wake-atomic-append",
                    crate::ProcessEventAppendRequest::new(
                        "producer.atomic",
                        serde_json::json!({"wake_input": "atomic"}),
                    )
                    .with_wake_target_scope(crate::SessionScope::new("atomic-target")),
                )
                .await
        });
        pause.wait_until_validated().await;
        let scan_registry = Arc::clone(&registry);
        let scan =
            crate::task::spawn(async move { scan_registry.pending_wake_deliveries(32).await });
        tokio::task::yield_now().await;
        assert!(
            !scan.is_finished(),
            "outbox scan must not observe the staged row before event and fold commit"
        );
        pause.resume();
        let appended = append
            .await
            .expect("join atomic append")
            .expect("commit atomic append");
        let deliveries = scan
            .await
            .expect("join atomic outbox scan")
            .expect("scan committed outbox");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].wake.sequence, appended.event.sequence);
        assert_eq!(
            registry
                .events_after("wake-atomic-append", 0)
                .await
                .expect("read committed event")
                .len(),
            1
        );
    }
}
