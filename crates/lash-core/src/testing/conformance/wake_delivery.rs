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

async fn consume_queued_batch(
    store: &Arc<dyn crate::RuntimePersistence>,
    session_id: &str,
    batch_id: &str,
    owner_tag: &str,
) {
    let owner = crate::LeaseOwnerIdentity::opaque(owner_tag, "wake-conformance");
    let lease = match store
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim wake target session")
    {
        crate::SessionExecutionLeaseClaimOutcome::Acquired(lease) => lease,
        crate::SessionExecutionLeaseClaimOutcome::Busy { holder } => {
            panic!("wake target lease must be available: {holder:?}")
        }
    };
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &[batch_id.to_string()],
        )
        .await
        .expect("claim queued wake")
        .expect("queued wake claim");
    let state = crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("load target state before queue settlement")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: session_id.to_string(),
            ..crate::RuntimeSessionState::default()
        });
    store
        .commit_runtime_state(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion())
                .releasing_session_execution_lease(lease.completion()),
        )
        .await
        .expect("settle queued wake and advance high water");
}

async fn wait_until_delivery_due(registry: &Arc<dyn crate::ProcessRegistry>, delivery_id: &str) {
    let next_attempt_at_ms = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("list deferred wake")
        .into_iter()
        .find(|delivery| delivery.delivery_id == delivery_id)
        .expect("deferred wake remains pending")
        .next_attempt_at_ms;
    let wait_ms = next_attempt_at_ms
        .saturating_sub(crate::current_epoch_ms())
        .saturating_add(5);
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
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

    // Terminal completion must route its planned wake through the completion
    // transaction, not only through generic append_event.
    let terminal_wake_type = crate::ProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec {
            terminal: Some(crate::ProcessTerminalSpec {
                state: crate::ProcessTerminalState::Completed,
                await_output: Some(crate::ProcessValueSelector::Pointer(
                    "/await_output".to_string(),
                )),
            }),
            wake: Some(crate::ProcessWakeSpec {
                when: None,
                input: crate::ProcessValueSelector::Pointer(
                    "/await_output/value/wake_input".to_string(),
                ),
            }),
        },
    };
    registry
        .register_process(
            process_registry::registration("wake-terminal-event")
                .with_event_types([terminal_wake_type])
                .with_wake_target(Some(crate::SessionScope::new(&target_request.session_id))),
        )
        .await
        .expect("register terminal wake producer");
    registry
        .complete_process(
            "wake-terminal-event",
            crate::ProcessAwaitOutput::Success {
                value: serde_json::json!({"wake_input": "terminal wake"}),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete terminal wake producer");
    let terminal_delivery = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("list terminal wake outbox")
        .into_iter()
        .find(|delivery| delivery.wake.process_id == "wake-terminal-event")
        .expect("completion path commits terminal wake outbox row");
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    let terminal_batch = target
        .list_queued_work(&target_request.session_id)
        .await
        .expect("list completion-path wake")
        .into_iter()
        .find(|batch| batch.source_key.as_deref() == Some(terminal_delivery.source_key().as_str()))
        .expect("completion-path wake is enqueued");
    consume_queued_batch(
        &target,
        &target_request.session_id,
        &terminal_batch.batch_id,
        "wake-terminal-owner",
    )
    .await;
    registry
        .prune_terminal_processes(u64::MAX, None, None)
        .await
        .expect("prune terminal producer and cascaded delivery rows");
    target
        .enqueue_queued_work(crate::process_wake_batch_draft(
            terminal_delivery.wake.clone(),
        ))
        .await
        .expect("producer pruning cannot erase receiver high water");
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after terminal producer prune")
            .iter()
            .all(|batch| {
                batch.source_key.as_deref() != Some(terminal_delivery.source_key().as_str())
            }),
        "receiver high water must survive sender process and delivery pruning"
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
    let mut ordered_deliveries = registry
        .list_wake_deliveries(None)
        .await
        .expect("list ordered delivery rows")
        .into_iter()
        .filter(|delivery| delivery.wake.process_id == "wake-ordered")
        .collect::<Vec<_>>();
    ordered_deliveries.sort_by_key(|delivery| delivery.wake.sequence);
    assert_eq!(ordered_deliveries.len(), 2);
    for delivery in &ordered_deliveries {
        let batch = target
            .enqueue_queued_work(crate::process_wake_batch_draft(delivery.wake.clone()))
            .await
            .expect("enqueue ordered wake structurally");
        consume_queued_batch(
            &target,
            &target_request.session_id,
            &batch.batch_id,
            &format!("wake-ordered-{}", delivery.wake.sequence),
        )
        .await;
        registry
            .mark_wake_enqueued(&delivery.delivery_id)
            .await
            .expect("settle ordered sender delivery");
    }
    for delivery in &ordered_deliveries {
        target
            .enqueue_queued_work(crate::process_wake_batch_draft(delivery.wake.clone()))
            .await
            .expect("one monotone high water absorbs the consumed prefix");
    }
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after ordered prefix redelivery")
            .iter()
            .all(
                |batch| batch.source_key.as_deref().is_none_or(|source_key| {
                    ordered_deliveries
                        .iter()
                        .all(|delivery| source_key != delivery.source_key())
                })
            ),
        "one high-water row must absorb every sequence in the consumed prefix"
    );

    // A discarded lower sequence must block later delivery until the host
    // redrives it. Otherwise a later consumption could advance high water
    // across a gap and silently absorb the never-consumed lower wake.
    registry
        .register_process(
            process_registry::registration("wake-discarded-head").with_extra_event_types([
                process_registry::wake_event_type("producer.discarded_head"),
            ]),
        )
        .await
        .expect("register discarded-head wake producer");
    for value in ["first", "second"] {
        registry
            .append_event(
                "wake-discarded-head",
                crate::ProcessEventAppendRequest::new(
                    "producer.discarded_head",
                    serde_json::json!({"wake_input": value}),
                )
                .with_wake_target_scope(crate::SessionScope::new(&target_request.session_id)),
            )
            .await
            .expect("append discarded-head wake");
    }
    let mut discarded_head_deliveries = registry
        .list_wake_deliveries(None)
        .await
        .expect("list discarded-head deliveries")
        .into_iter()
        .filter(|delivery| delivery.wake.process_id == "wake-discarded-head")
        .collect::<Vec<_>>();
    discarded_head_deliveries.sort_by_key(|delivery| delivery.wake.sequence);
    let [first, second] = discarded_head_deliveries.as_slice() else {
        panic!("discarded-head process must have exactly two deliveries");
    };
    registry
        .discard_wake_delivery(&first.delivery_id, crate::WakeDiscardReason::Expired)
        .await
        .expect("discard lower group head");
    assert!(
        registry
            .pending_wake_deliveries(64)
            .await
            .expect("scan behind discarded group head")
            .iter()
            .all(|delivery| delivery.wake.process_id != "wake-discarded-head"),
        "a discarded lower sequence must block later sequences in its group"
    );
    registry
        .redrive_wake_delivery(&first.delivery_id)
        .await
        .expect("redrive discarded group head");
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    let first_batch = target
        .list_queued_work(&target_request.session_id)
        .await
        .expect("list redriven lower sequence")
        .into_iter()
        .find(|batch| batch.source_key.as_deref() == Some(first.source_key().as_str()))
        .expect("redriven lower sequence is delivered first");
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("list before lower sequence consumption")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(second.source_key().as_str())),
        "one scan must enqueue only the redriven group head"
    );
    consume_queued_batch(
        &target,
        &target_request.session_id,
        &first_batch.batch_id,
        "wake-discarded-head-first",
    )
    .await;
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("list after discarded head redrive")
            .iter()
            .any(|batch| batch.source_key.as_deref() == Some(second.source_key().as_str())),
        "later sequence may deliver after the redriven lower sequence"
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
    consume_queued_batch(
        &target,
        &target_request.session_id,
        &before_mark_batch.batch_id,
        "wake-crash-owner",
    )
    .await;
    let consumed_replay = target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("receiver high water absorbs redelivery while sender remains pending");
    assert_ne!(consumed_replay.batch_id, before_mark_batch.batch_id);
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue while consumed high water is retained")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key()))
    );
    registry
        .discard_wake_delivery(
            &before_mark.delivery_id,
            crate::WakeDiscardReason::TargetGone,
        )
        .await
        .expect("simulate a terminal sender after receiver consumption");
    registry
        .redrive_wake_delivery(&before_mark.delivery_id)
        .await
        .expect("host redrive consumed delivery");
    drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after consumed redelivery")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key())),
        "late sender redelivery must resolve against receiver high water"
    );
    target
        .enqueue_queued_work(crate::process_wake_batch_draft(before_mark.wake.clone()))
        .await
        .expect("stale driver resolves against permanent high water");
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("queue after stale terminal snapshot")
            .iter()
            .all(|batch| batch.source_key.as_deref() != Some(&before_mark.source_key())),
        "a stale driver must never recreate consumed work after sender terminality"
    );
    // Competing drivers may both inspect a pending row, but queue and sender
    // transitions remain idempotent.
    let concurrent = append_wake(
        &registry,
        "wake-concurrent-drivers",
        &target_request.session_id,
    )
    .await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_registry = Arc::clone(&registry);
    let left_factory = Arc::clone(&factory);
    let left_barrier = Arc::clone(&barrier);
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        drive_once(left_registry, left_factory).await
    });
    let right_registry = Arc::clone(&registry);
    let right_factory = Arc::clone(&factory);
    let right_barrier = Arc::clone(&barrier);
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        drive_once(right_registry, right_factory).await
    });
    barrier.wait().await;
    let (left, right) = (
        left.await.expect("join left wake driver"),
        right.await.expect("join right wake driver"),
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

    // A never-created receiver backs off without starving another group.
    let future_target_id = "future-wake-target";
    let future = append_wake(&registry, "wake-future-target", future_target_id).await;
    let healthy = append_wake(
        &registry,
        "wake-healthy-beside-future",
        &target_request.session_id,
    )
    .await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert_eq!(report.discarded_target_gone, 0);
    assert!(
        target
            .list_queued_work(&target_request.session_id)
            .await
            .expect("healthy queue beside missing target")
            .iter()
            .any(|batch| batch.source_key.as_deref() == Some(&healthy.source_key())),
        "a deferred group must not starve a healthy group in the same pass"
    );
    let deferred = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("pending future-target deliveries")
        .into_iter()
        .find(|delivery| delivery.delivery_id == future.delivery_id)
        .expect("future-target delivery remains pending");
    assert_eq!(
        deferred.attempts, 1,
        "the missing target is inspected once before entering backoff"
    );
    assert!(
        deferred.next_attempt_at_ms
            > deferred
                .first_attempt_ms
                .expect("the first failed attempt records its timestamp"),
        "retry scheduling must move the delivery's due time forward"
    );
    let future_target = factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: future_target_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: target_request.policy.clone(),
        })
        .await
        .expect("create future wake target");
    wait_until_delivery_due(&registry, &future.delivery_id).await;
    let report = drive_once(Arc::clone(&registry), Arc::clone(&factory)).await;
    assert!(report.enqueued >= 1);
    assert_eq!(
        registry
            .list_wake_deliveries(None)
            .await
            .expect("list future-target delivery after target creation")
            .into_iter()
            .find(|delivery| delivery.delivery_id == future.delivery_id)
            .expect("future-target delivery remains observable")
            .state,
        crate::WakeDeliveryState::Enqueued,
        "the previously missing target's own delivery must become enqueued"
    );
    assert!(
        future_target
            .list_queued_work(future_target_id)
            .await
            .expect("list future target queue")
            .iter()
            .any(|batch| batch.source_key.as_deref() == Some(&future.source_key())),
        "the previously missing target must receive its own queued wake"
    );

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
    async fn discard_race_converges_without_destructive_compensation() {
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
            .expect("settle stale enqueue against terminal sender");
        let live = target
            .list_queued_work(&request.session_id)
            .await
            .expect("list queue after discard race")
            .into_iter()
            .find(|batch| batch.source_key.as_deref() == Some(&delivery.source_key()))
            .expect("the non-destructive race winner leaves its safe live batch");
        consume_queued_batch(
            &target,
            &request.session_id,
            &live.batch_id,
            "wake-discard-race-owner",
        )
        .await;
        registry
            .redrive_wake_delivery(&delivery.delivery_id)
            .await
            .expect("redrive consumed discarded delivery");
        drive_once(registry_dyn, factory.clone()).await;
        assert!(
            target
                .list_queued_work(&request.session_id)
                .await
                .expect("list queue after consumed redrive")
                .iter()
                .all(|batch| batch.source_key.as_deref() != Some(&delivery.source_key())),
            "high water must absorb redrive without deleting an in-flight claim"
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
