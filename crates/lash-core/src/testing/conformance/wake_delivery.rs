use super::*;

#[derive(Debug)]
struct WakeDeliveryConformanceClock(u64);

#[async_trait::async_trait]
impl crate::Clock for WakeDeliveryConformanceClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.0
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.0))
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

/// Cross-backend wake-delivery crash contract.
///
/// A process append owns the outbox insertion. Delivery may happen on a later
/// host instance, must be idempotent, and must never mutate the lifecycle fold.
pub async fn wake_delivery_crash_matrix(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let target_session_id = "wake-crash-target";
    let request = crate::SessionStoreCreateRequest {
        session_id: target_session_id.to_string(),
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
            session_id: Some(target_session_id.to_string()),
            autonomous: false,
            max_turns: None,
            prompt: crate::PromptLayer::new(),
            generation: crate::GenerationOptions::default(),
        },
    };
    let target = factory
        .create_store(&request)
        .await
        .expect("create wake target");

    let process_id = "wake-append-only";
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register wake producer");
    let append = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "resume"}),
            ),
        )
        .await
        .expect("append wake event");
    let wake = append
        .wake_delivery
        .expect("append creates wake outbox row");
    assert_eq!(wake.target_session_id, target_session_id);
    let before = serde_json::to_vec(
        &registry
            .get_process(process_id)
            .await
            .expect("read producer after append")
            .expect("producer exists"),
    )
    .expect("serialize producer before delivery");
    assert!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("read target queue before recovery")
            .is_empty(),
        "process append must not pretend receiver enqueue already happened"
    );

    let first = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::new(crate::SystemClock),
        32,
    )
    .await
    .expect("recover pending wake");
    assert_eq!(first.enqueued, 1, "unexpected delivery report: {first:?}");
    let queued = target
        .list_queued_work(target_session_id)
        .await
        .expect("read target queue");
    let source_key = crate::process_wake_source_key(&wake.process_id, wake.sequence);
    assert_eq!(
        queued
            .iter()
            .filter(|item| item.source_key.as_deref() == Some(source_key.as_str()))
            .count(),
        1
    );

    let second = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::new(crate::SystemClock),
        32,
    )
    .await
    .expect("re-drive wake outbox");
    assert_eq!(second.enqueued, 0);
    let after = serde_json::to_vec(
        &registry
            .get_process(process_id)
            .await
            .expect("read producer after delivery")
            .expect("producer remains"),
    )
    .expect("serialize producer after delivery");
    assert_eq!(
        before, after,
        "wake outbox and delivery transitions changed lifecycle bytes"
    );

    let coalesced_process_id = "wake-coalesced-sender";
    registry
        .register_process(
            process_registry::registration(coalesced_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register coalesced wake producer");
    let mut coalesced_sequences = Vec::new();
    for wake_input in ["coalesced-a", "coalesced-b"] {
        coalesced_sequences.push(
            registry
                .append_event(
                    coalesced_process_id,
                    crate::ProcessEventAppendRequest::new(
                        "producer.wake",
                        serde_json::json!({"wake_input": wake_input}),
                    ),
                )
                .await
                .expect("append coalesced wake")
                .wake_delivery
                .expect("coalesced wake outbox row")
                .sequence,
        );
    }
    let coalesce_policy = crate::WakeTurnPolicy::coalesce(
        crate::DeliveryPolicy::EarliestSafeBoundary,
        crate::WakeCoalescingKey::Group("conformance-wakes".to_string()),
    );
    let mut coalesced_enqueued = 0;
    for _ in 0..2 {
        coalesced_enqueued += crate::WakeDeliveryDriver::drive_pending_once_with_policy(
            Arc::clone(&registry),
            Arc::clone(&factory),
            None,
            Arc::new(crate::SystemClock),
            &coalesce_policy,
            32,
        )
        .await
        .expect("deliver coalescing candidates")
        .enqueued;
    }
    assert_eq!(
        coalesced_enqueued, 2,
        "each sender outbox row must settle even when receiver claims can merge"
    );
    let deliveries = registry
        .list_wake_deliveries(None)
        .await
        .expect("list coalesced sender rows");
    for sequence in coalesced_sequences {
        assert_eq!(
            deliveries
                .iter()
                .find(|delivery| {
                    delivery.wake.process_id == coalesced_process_id
                        && delivery.wake.sequence == sequence
                })
                .expect("coalesced sender row remains inspectable")
                .state,
            crate::WakeDeliveryState::Enqueued,
            "every sender row must be settled independently"
        );
    }
    let coalesced_receiver_rows = target
        .list_queued_work(target_session_id)
        .await
        .expect("list coalesced receiver rows")
        .into_iter()
        .filter(|batch| {
            batch
                .source_key
                .as_deref()
                .is_some_and(|key| key.contains(coalesced_process_id))
        })
        .collect::<Vec<_>>();
    assert_eq!(coalesced_receiver_rows.len(), 2);
    assert!(coalesced_receiver_rows.iter().all(|batch| {
        batch.slot_policy == crate::SlotPolicy::Join
            && batch.merge_key == crate::MergeKey::Group("conformance-wakes".to_string())
    }));

    let retarget_process_id = "wake-retarget-in-flight";
    registry
        .register_process(
            process_registry::registration(retarget_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register retarget-race producer");
    let retarget_wake = registry
        .append_event(
            retarget_process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "retarget-race"}),
            ),
        )
        .await
        .expect("append retarget-race wake")
        .wake_delivery
        .expect("retarget-race wake delivery");
    let claimed = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim retarget-race wake");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].state, crate::WakeDeliveryState::Enqueuing);
    registry
        .retarget_subscription(retarget_process_id, Some("wake-new-target"))
        .await
        .expect("retarget while delivery is in flight");
    target
        .enqueue_queued_work(crate::process_wake_batch_draft(claimed[0].wake.clone()))
        .await
        .expect("enqueue claimed wake to original target");
    assert_eq!(
        registry
            .mark_wake_enqueued(
                &claimed[0].delivery_id,
                claimed[0].claim_token().expect("retarget claim token"),
            )
            .await
            .expect("settle claimed wake truthfully"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    let retarget_delivery = registry
        .list_wake_deliveries(None)
        .await
        .expect("read settled retarget-race wake")
        .into_iter()
        .find(|delivery| delivery.delivery_id == claimed[0].delivery_id)
        .expect("settled retarget-race delivery");
    assert_eq!(retarget_delivery.state, crate::WakeDeliveryState::Enqueued);
    assert_eq!(retarget_delivery.wake.target_session_id, target_session_id);
    assert_eq!(retarget_delivery.discard_reason, None);
    let retarget_source =
        crate::process_wake_source_key(&retarget_wake.process_id, retarget_wake.sequence);
    assert_eq!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("read original target after retarget race")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(retarget_source.as_str()))
            .count(),
        1,
        "an in-flight retarget race must leave one truthful queued turn"
    );

    let crash_process_id = "wake-crashed-enqueuing";
    registry
        .register_process(
            process_registry::registration(crash_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register stale-claim producer");
    let crash_wake = registry
        .append_event(
            crash_process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "recover-stale-claim"}),
            ),
        )
        .await
        .expect("append stale-claim wake")
        .wake_delivery
        .expect("stale-claim wake delivery");
    let crashed_claim = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim wake before simulated crash");
    assert_eq!(crashed_claim.len(), 1);
    assert_eq!(crashed_claim[0].state, crate::WakeDeliveryState::Enqueuing);
    let stale_token = crashed_claim[0]
        .claim_token()
        .expect("first claim token")
        .to_string();
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let recovered_claim = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("recover stale enqueuing claim");
    assert_eq!(recovered_claim.len(), 1);
    let recovered = &recovered_claim[0];
    assert_ne!(
        recovered.claim_token().expect("recovered claim token"),
        stale_token,
        "reclaim must mint a fresh ownership fence"
    );
    assert_eq!(
        registry
            .defer_wake_delivery(
                &crashed_claim[0].delivery_id,
                &stale_token,
                crashed_claim[0].next_attempt_at_ms.saturating_add(1),
            )
            .await
            .expect("stale driver defer is a benign no-op"),
        crate::WakeDeliveryClaimOutcome::ClaimLost {
            state: crate::WakeDeliveryState::Enqueuing,
        }
    );
    registry
        .retarget_subscription(crash_process_id, Some("wake-after-reclaim-target"))
        .await
        .expect("retarget while recovered claim is in flight");
    target
        .enqueue_queued_work(crate::process_wake_batch_draft(recovered.wake.clone()))
        .await
        .expect("recovered owner enqueues to its claimed target");
    assert_eq!(
        registry
            .mark_wake_enqueued(
                &recovered.delivery_id,
                recovered.claim_token().expect("recovered claim token"),
            )
            .await
            .expect("recovered owner settles truthfully"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    let crash_source = crate::process_wake_source_key(&crash_wake.process_id, crash_wake.sequence);
    assert_eq!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("read queue after stale-claim recovery")
            .iter()
            .filter(|batch| batch.source_key.as_deref() == Some(crash_source.as_str()))
            .count(),
        1,
        "a stale enqueuing claim must recover to exactly one queued turn"
    );

    let blocked_process_id = "wake-discarded-group-head";
    registry
        .register_process(
            process_registry::registration(blocked_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register blocked-group producer");
    let mut blocked_wakes = Vec::new();
    for wake_input in ["blocked-head", "blocked-tail"] {
        blocked_wakes.push(
            registry
                .append_event(
                    blocked_process_id,
                    crate::ProcessEventAppendRequest::new(
                        "producer.wake",
                        serde_json::json!({"wake_input": wake_input}),
                    ),
                )
                .await
                .expect("append blocked-group wake")
                .wake_delivery
                .expect("blocked-group wake delivery"),
        );
    }
    let blocked_head = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim blocked-group head")
        .into_iter()
        .next()
        .expect("blocked-group head is claimable");
    assert_eq!(blocked_head.wake.sequence, blocked_wakes[0].sequence);
    assert_eq!(
        registry
            .discard_wake_delivery(
                &blocked_head.delivery_id,
                blocked_head
                    .claim_token()
                    .expect("blocked-head claim token"),
                crate::WakeDiscardReason::Expired,
            )
            .await
            .expect("discard blocked-group head"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    assert!(
        registry
            .claim_pending_wake_deliveries(1)
            .await
            .expect("scan blocked group")
            .is_empty(),
        "a discarded head must retain the ordering block"
    );
    let delivery_report = registry
        .wake_delivery_report()
        .await
        .expect("report blocked wake group");
    let blocked_group = delivery_report
        .blocked_groups
        .iter()
        .find(|group| group.process_id == blocked_process_id)
        .expect("blocked group must be visible in the delivery report");
    assert_eq!(blocked_group.target_session_id, target_session_id);
    assert_eq!(blocked_group.blocking_delivery_id, blocked_head.delivery_id);
    assert_eq!(blocked_group.reason, crate::WakeDiscardReason::Expired);
    assert_eq!(
        blocked_group.redrive_delivery_id, blocked_head.delivery_id,
        "the report must name the actionable redrive lever"
    );
    registry
        .redrive_wake_delivery(&blocked_group.redrive_delivery_id)
        .await
        .expect("reported redrive lever unblocks the group");
    assert!(
        registry
            .wake_delivery_report()
            .await
            .expect("report after redrive")
            .blocked_groups
            .iter()
            .all(|group| group.process_id != blocked_process_id),
        "redriving the named head must clear the blocked-group report"
    );

    target_gone_is_a_typed_discard(Arc::clone(&factory), Arc::clone(&registry)).await;
    expired_is_a_typed_discard(factory, registry).await;
}

async fn target_gone_is_a_typed_discard(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let target_session_id = "wake-target-gone-session";
    let target_request = crate::SessionStoreCreateRequest {
        session_id: target_session_id.to_string(),
        relation: crate::SessionRelation::Root,
        policy: crate::SessionPolicy::default(),
    };
    factory
        .create_store(&target_request)
        .await
        .expect("create target-gone wake target");
    factory
        .delete_session(target_session_id)
        .await
        .expect("tombstone target-gone wake target");

    let process_id = "wake-target-gone-sender";
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register target-gone wake sender");
    let wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "target-gone"}),
            ),
        )
        .await
        .expect("append target-gone wake")
        .wake_delivery
        .expect("target-gone wake delivery");
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        factory,
        None,
        Arc::new(crate::SystemClock),
        32,
    )
    .await
    .expect("drive target-gone wake");
    assert_eq!(report.discarded_target_gone, 1);
    let delivery = registry
        .list_wake_deliveries(None)
        .await
        .expect("list target-gone wake")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == wake.process_id && delivery.wake.sequence == wake.sequence
        })
        .expect("target-gone wake remains inspectable");
    assert_eq!(delivery.state, crate::WakeDeliveryState::Discarded);
    assert_eq!(
        delivery.discard_reason,
        Some(crate::WakeDiscardReason::TargetGone)
    );
}

async fn expired_is_a_typed_discard(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
) {
    let target_session_id = "wake-crash-target";
    let process_id = "wake-expired-sender";
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register expiring wake sender");
    let wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "expired"}),
            ),
        )
        .await
        .expect("append expiring wake")
        .wake_delivery
        .expect("expiring wake delivery");
    let expires_at_ms = registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("list pending expiring wake")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == wake.process_id && delivery.wake.sequence == wake.sequence
        })
        .expect("expiring wake remains pending")
        .expires_at_ms;
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        factory,
        None,
        Arc::new(WakeDeliveryConformanceClock(expires_at_ms)),
        32,
    )
    .await
    .expect("drive expired wake with injected clock");
    assert!(
        report.discarded_expired >= 1,
        "the injected expiry clock must discard at least the named wake: {report:?}"
    );
    let delivery = registry
        .list_wake_deliveries(None)
        .await
        .expect("list expired wake")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == wake.process_id && delivery.wake.sequence == wake.sequence
        })
        .expect("expired wake remains inspectable");
    assert_eq!(delivery.state, crate::WakeDeliveryState::Discarded);
    assert_eq!(
        delivery.discard_reason,
        Some(crate::WakeDiscardReason::Expired)
    );
}
