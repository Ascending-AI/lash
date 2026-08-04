use super::*;
use crate::testing::TestClock;

#[derive(Default)]
struct RecordingWakeTurnHandle {
    runs: tokio::sync::Mutex<Vec<crate::QueuedWorkRunRequest>>,
    notify: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::QueuedWorkRunHandle for RecordingWakeTurnHandle {
    async fn run_queued_work(
        &self,
        request: crate::QueuedWorkRunRequest,
    ) -> Result<(), crate::QueuedWorkRunError> {
        self.runs.lock().await.push(request);
        self.notify.notify_one();
        Ok(())
    }
}

impl RecordingWakeTurnHandle {
    async fn wait_for_process_wake(&self, session_id: &str, prior_runs: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self.runs.lock().await.iter().skip(prior_runs).any(|run| {
                    run.session_id.as_deref() == Some(session_id) && run.reason == "process_wake"
                }) {
                    return;
                }
                self.notify.notified().await;
            }
        })
        .await
        .expect("process wake must fire the queued-work turn driver");
    }

    async fn len(&self) -> usize {
        self.runs.lock().await.len()
    }
}

/// Cross-backend wake-delivery crash contract.
///
/// A process append owns the outbox insertion. Delivery may happen on a later
/// host instance, must be idempotent, and must never mutate the lifecycle fold.
pub async fn wake_delivery_crash_matrix(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
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
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
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
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
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

    prune_reregister_sender_floor_delivers_through_driver(
        Arc::clone(&factory),
        Arc::clone(&registry),
        Arc::clone(&clock),
        Arc::clone(&target),
        target_session_id,
    )
    .await;
    replay_and_same_millisecond_allocation_are_deterministic(
        Arc::clone(&registry),
        Arc::clone(&clock),
    )
    .await;
    mixed_era_floor_and_ordering(
        Arc::clone(&factory),
        Arc::clone(&registry),
        Arc::clone(&clock),
        Arc::clone(&target),
        target_session_id,
    )
    .await;
    rewound_fresh_delivery_is_discarded_without_blocking(
        Arc::clone(&factory),
        Arc::clone(&registry),
        Arc::clone(&clock),
        Arc::clone(&target),
        target_session_id,
    )
    .await;

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
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
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
    clock.advance(40);
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

    let settled_crash_process_id = "wake-crashed-after-live-receiver-enqueue";
    registry
        .register_process(
            process_registry::registration(settled_crash_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register live-row retry producer");
    let settled_crash_wake = registry
        .append_event(
            settled_crash_process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "live receiver row before sender mark"}),
            ),
        )
        .await
        .expect("append settled crash-window wake")
        .wake_delivery
        .expect("settled crash-window delivery");
    let settled_crash_claim = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim settled crash-window wake")
        .into_iter()
        .next()
        .expect("settled crash-window wake is claimable");
    let receiver_batch = target
        .enqueue_queued_work(crate::process_wake_batch_draft(
            settled_crash_claim.wake.clone(),
        ))
        .await
        .expect("receiver accepts settled crash-window wake");
    assert!(receiver_batch.enqueue_seq > 0);
    clock.advance(40);
    let settled_crash_report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("recover sender claim against live receiver row");
    assert_eq!(settled_crash_report.enqueued, 1);
    assert_eq!(settled_crash_report.floor_absorbed, 1);
    assert_eq!(settled_crash_report.discarded_sequence_rewound, 0);
    let settled_sender = registry
        .list_wake_deliveries(None)
        .await
        .expect("list settled crash-window sender row")
        .into_iter()
        .find(|delivery| delivery.wake.process_id == settled_crash_process_id)
        .expect("settled crash-window sender row remains");
    assert_eq!(settled_sender.wake.sequence, settled_crash_wake.sequence);
    assert_eq!(settled_sender.state, crate::WakeDeliveryState::Enqueued);

    let deferred_process_id = "wake-deferred-before-first-receiver-attempt";
    registry
        .register_process(
            process_registry::registration(deferred_process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register deferred-first-attempt producer");
    registry
        .append_event(
            deferred_process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "deferred before receiver call"}),
            ),
        )
        .await
        .expect("append deferred-first-attempt wake");
    let deferred = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("claim deferred-first-attempt wake")
        .into_iter()
        .next()
        .expect("deferred-first-attempt wake is claimable");
    assert_eq!(deferred.attempts, 1);
    let retry_at = crate::Clock::timestamp_ms(clock.as_ref()).saturating_add(50);
    assert_eq!(
        registry
            .defer_wake_delivery(
                &deferred.delivery_id,
                deferred.claim_token().expect("deferred claim token"),
                retry_at,
            )
            .await
            .expect("defer before the first receiver call"),
        crate::WakeDeliveryClaimOutcome::Applied
    );
    clock.advance(50);
    let deferred_report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("retry deferred fresh wake");
    assert_eq!(deferred_report.enqueued, 1);
    assert_eq!(deferred_report.floor_absorbed, 0);
    assert_eq!(deferred_report.discarded_sequence_rewound, 0);
    assert!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("list receiver rows after deferred fresh retry")
            .iter()
            .any(|batch| {
                batch
                    .source_key
                    .as_deref()
                    .is_some_and(|source_key| source_key.contains(deferred_process_id))
            }),
        "deferred first receiver attempt must eventually create a live receiver row"
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

    sender_floor_lifetime(
        Arc::clone(&factory),
        Arc::clone(&registry),
        Arc::clone(&clock),
    )
    .await;

    target_gone_is_a_typed_discard(
        Arc::clone(&factory),
        Arc::clone(&registry),
        Arc::clone(&clock),
    )
    .await;
    expired_is_a_typed_discard(factory, registry, clock).await;
}

async fn sender_floor_lifetime(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
) {
    let target_session_id = "wake-allocation-floor-lifetime-target";
    let target = factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: target_session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        })
        .await
        .expect("create sender-floor lifetime target");
    let process_id = "wake-allocation-floor-lifetime-process";
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register sender-floor lifetime process");
    let wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "floor lifetime"}),
            ),
        )
        .await
        .expect("append sender-floor lifetime wake")
        .wake_delivery
        .expect("sender-floor lifetime wake delivery");
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        clock as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("deliver sender-floor lifetime wake");
    assert!(
        report.enqueued >= 1,
        "unexpected delivery report: {report:?}"
    );
    let batch = target
        .list_queued_work(target_session_id)
        .await
        .expect("list sender-floor lifetime receiver row")
        .into_iter()
        .find(|batch| {
            batch.source_key.as_deref()
                == Some(crate::process_wake_source_key(process_id, wake.sequence).as_str())
        })
        .expect("sender-floor lifetime wake reached receiver");
    settle_queued_batch(&target, target_session_id, &batch.batch_id).await;
    complete_and_prune(&registry, process_id).await;
    let retained_floor = registry
        .wake_allocation_floor_for_testing(target_session_id, process_id)
        .await
        .expect("read sender floor after process prune")
        .expect("process prune must retain sender floor");
    assert!(retained_floor > wake.sequence);
    registry
        .compact_process_tombstones(u64::MAX, crate::ProjectionWatermark::NoProjector, None)
        .await
        .expect("compact process tombstone");
    assert_eq!(
        registry
            .wake_allocation_floor_for_testing(target_session_id, process_id)
            .await
            .expect("read sender floor after tombstone compaction"),
        Some(retained_floor),
        "tombstone compaction must retain sender floor"
    );
    registry
        .delete_session_process_state(target_session_id)
        .await
        .expect("delete target-owned process state");
    factory
        .delete_session(target_session_id)
        .await
        .expect("delete sender-floor lifetime target");
    assert_eq!(
        registry
            .wake_allocation_floor_for_testing(target_session_id, process_id)
            .await
            .expect("read sender floor after target deletion"),
        None,
        "target deletion must remove sender and receiver fences together"
    );
}

async fn settle_queued_batch(
    target: &Arc<dyn crate::RuntimePersistence>,
    session_id: &str,
    batch_id: &str,
) {
    let owner = crate::LeaseOwnerIdentity::opaque(
        format!("{batch_id}:owner"),
        format!("{batch_id}:incarnation"),
    );
    let lease = target
        .try_claim_session_execution_lease(session_id, &owner, 60_000)
        .await
        .expect("claim target session lease")
        .acquired()
        .expect("target session lease available");
    let claim = target
        .claim_ready_queued_work_by_batch_ids(
            session_id,
            &lease.fence(),
            &owner,
            crate::QueuedWorkClaimBoundary::Idle,
            &[batch_id.to_string()],
        )
        .await
        .expect("claim target wake batch")
        .expect("target wake batch remains live");
    let head_revision = target
        .load_session()
        .await
        .expect("load target session before wake settlement")
        .map_or(0, |read| read.head_revision);
    let (commit, _) = crate::RuntimeCommit::persisted_state_for_test(
        &crate::RuntimeSessionState {
            session_id: session_id.to_string(),
            head_revision,
            ..crate::RuntimeSessionState::default()
        },
        &[],
    )
    .with_operation(crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!("settle-wake:{batch_id}")),
        "commit",
    ))
    .expect("stamp unique wake-settlement operation");
    target
        .commit_runtime_state(
            commit
                .releasing_session_execution_lease(lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .expect("settle target wake batch");
}

async fn complete_and_prune(registry: &Arc<dyn crate::ProcessRegistry>, process_id: &str) {
    registry
        .complete_process(
            process_id,
            crate::ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old process incarnation");
    let (_, cursor) = registry
        .processes_changed_since(crate::ProcessChangeCursor::initial(), 10_000)
        .await
        .expect("read terminal process cursor");
    let report = registry
        .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::UpTo(cursor))
        .await
        .expect("prune old process incarnation");
    assert_eq!(report.pruned_processes, 1);
}

async fn prune_reregister_sender_floor_delivers_through_driver(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    target: Arc<dyn crate::RuntimePersistence>,
    target_session_id: &str,
) {
    clock.set(1_800_000_010_000);
    let process_id = "wake-floor-prune-reregister";
    let registration = || {
        process_registry::registration(process_id)
            .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
            .with_wake_session_id(Some(target_session_id.to_string()))
    };
    registry
        .register_process(registration())
        .await
        .expect("register old sender-floor wake producer");
    let old_wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "old incarnation"}),
            )
            .with_replay_key("wake-floor-prune-reregister:old"),
        )
        .await
        .expect("append old sender-floor wake")
        .wake_delivery
        .expect("old sender-floor wake delivery");
    let old_report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("deliver old sender-floor wake");
    assert_eq!(old_report.enqueued, 1);
    let old_sender_id = registry
        .list_wake_deliveries(None)
        .await
        .expect("list old sender-floor row")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == process_id && delivery.wake.sequence == old_wake.sequence
        })
        .expect("old sender-floor row")
        .delivery_id;
    let old_batch = target
        .list_queued_work(target_session_id)
        .await
        .expect("list old sender-floor wake")
        .into_iter()
        .find(|batch| {
            batch.source_key.as_deref()
                == Some(crate::process_wake_source_key(process_id, old_wake.sequence).as_str())
        })
        .expect("old sender-floor wake reached receiver");
    settle_queued_batch(&target, target_session_id, &old_batch.batch_id).await;
    complete_and_prune(&registry, process_id).await;
    assert!(
        registry
            .list_wake_deliveries(None)
            .await
            .expect("list sender rows after process prune")
            .iter()
            .all(|delivery| delivery.delivery_id != old_sender_id),
        "process prune must cascade every old-incarnation sender row"
    );

    registry
        .register_process(registration())
        .await
        .expect("re-register wake producer under frozen clock");
    let new_wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "new incarnation"}),
            )
            .with_replay_key("wake-floor-prune-reregister:new"),
        )
        .await
        .expect("append new-incarnation wake under frozen clock")
        .wake_delivery
        .expect("new-incarnation sender-floor wake delivery");
    assert!(new_wake.sequence > old_wake.sequence);
    let new_sender = registry
        .list_wake_deliveries(None)
        .await
        .expect("list sender outbox")
        .into_iter()
        .find(|delivery| {
            delivery.wake.process_id == process_id && delivery.wake.sequence == new_wake.sequence
        })
        .expect("new wake must create a sender outbox row");
    assert_ne!(
        new_sender.delivery_id, old_sender_id,
        "new wake must not collide with the old-incarnation delivery id"
    );
    assert_eq!(new_sender.state, crate::WakeDeliveryState::Pending);

    let turn_handle = Arc::new(RecordingWakeTurnHandle::default());
    let prior_runs = turn_handle.len().await;
    let queued_work_driver = crate::QueuedWorkDriver::new(
        Arc::clone(&turn_handle) as Arc<dyn crate::QueuedWorkRunHandle>
    );
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        factory,
        Some(queued_work_driver.clone()),
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("deliver re-registered sender-floor wake");
    assert_eq!(
        report.enqueued, 1,
        "new-incarnation wake must enqueue: {report:?}"
    );
    turn_handle
        .wait_for_process_wake(target_session_id, prior_runs)
        .await;
    assert!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("list new-incarnation receiver queue")
            .iter()
            .any(|batch| {
                batch.source_key.as_deref()
                    == Some(crate::process_wake_source_key(process_id, new_wake.sequence).as_str())
            }),
        "new-incarnation wake must survive sender and receiver dedupe doors"
    );
}

async fn replay_and_same_millisecond_allocation_are_deterministic(
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
) {
    clock.set(1_800_000_020_000);
    let process_id = "wake-floor-replay";
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([process_registry::plain_event_type("producer.progress")]),
        )
        .await
        .expect("register replay-allocation process");
    let request =
        crate::ProcessEventAppendRequest::new("producer.progress", serde_json::json!({"value": 1}))
            .with_replay_key("wake-floor-replay:stable");
    let first = registry
        .append_event(process_id, request.clone())
        .await
        .expect("append replay-allocation event");
    let same_ms = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.progress",
                serde_json::json!({"value": 2}),
            )
            .with_replay_key("wake-floor-replay:same-ms"),
        )
        .await
        .expect("append second event under frozen clock");
    assert_eq!(same_ms.event.sequence, first.event.sequence + 1);
    clock.advance(10_000);
    let replay = registry
        .append_event(process_id, request)
        .await
        .expect("replay event after clock advance");
    assert_eq!(
        serde_json::to_value(replay.event).expect("encode replayed event"),
        serde_json::to_value(first.event).expect("encode first event"),
        "replay must return the journaled sequence"
    );
}

async fn mixed_era_floor_and_ordering(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    target: Arc<dyn crate::RuntimePersistence>,
    target_session_id: &str,
) {
    let process_id = "wake-floor-mixed-era";
    let mut dense_batches = Vec::new();
    for sequence in 1..=3 {
        let wake = crate::ProcessWakeDelivery {
            wake_id: format!("wake:mixed-era:{sequence}"),
            target_session_id: target_session_id.to_string(),
            process_id: process_id.to_string(),
            sequence,
            event_type: "producer.wake".to_string(),
            event_invocation: crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new(target_session_id),
                format!("wake:mixed-era:{sequence}"),
                crate::RuntimeEffectKind::Process,
                format!("wake:mixed-era:{sequence}"),
            ),
            process_caused_by: None,
            input: format!("old dense wake {sequence}"),
            created_at_ms: sequence,
        };
        dense_batches.push(
            target
                .enqueue_queued_work(crate::process_wake_batch_draft(wake))
                .await
                .expect("enqueue old dense wake"),
        );
    }
    settle_queued_batch(&target, target_session_id, &dense_batches[2].batch_id).await;
    let settled_redelivery = target
        .enqueue_queued_work(crate::process_wake_batch_draft(
            crate::ProcessWakeDelivery {
                wake_id: "wake:mixed-era:3".to_string(),
                target_session_id: target_session_id.to_string(),
                process_id: process_id.to_string(),
                sequence: 3,
                event_type: "producer.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::RuntimeScope::new(target_session_id),
                    "wake:mixed-era:3",
                    crate::RuntimeEffectKind::Process,
                    "wake:mixed-era:3",
                ),
                process_caused_by: None,
                input: "old dense wake 3".to_string(),
                created_at_ms: 3,
            },
        ))
        .await
        .expect_err("settled no-live-row wake must trip the receiver floor");
    assert!(
        matches!(
            settled_redelivery,
            crate::StoreError::ProcessWakeSequenceRewound {
                sequence: 3,
                allocation_floor: 3,
                ..
            }
        ),
        "settled floor returned the wrong typed error: {settled_redelivery}"
    );
    let live_redelivery = target
        .enqueue_queued_work(crate::process_wake_batch_draft(
            crate::ProcessWakeDelivery {
                wake_id: "wake:mixed-era:2".to_string(),
                target_session_id: target_session_id.to_string(),
                process_id: process_id.to_string(),
                sequence: 2,
                event_type: "producer.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::RuntimeScope::new(target_session_id),
                    "wake:mixed-era:2",
                    crate::RuntimeEffectKind::Process,
                    "wake:mixed-era:2",
                ),
                process_caused_by: None,
                input: "old dense wake 2".to_string(),
                created_at_ms: 2,
            },
        ))
        .await
        .expect("dedupe live old dense wake");
    assert_eq!(live_redelivery.batch_id, dense_batches[1].batch_id);

    clock.set(1_800_000_030_000);
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([
                    process_registry::plain_event_type("producer.progress"),
                    process_registry::wake_event_type("producer.wake"),
                ])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register mixed-era process");
    for value in 1..=3 {
        registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::new(
                    "producer.progress",
                    serde_json::json!({"value": value}),
                ),
            )
            .await
            .expect("append dense sender-floor predecessor");
    }
    let sender_floor_wake = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "timestamp era"}),
            ),
        )
        .await
        .expect("append small sender-floor wake")
        .wake_delivery
        .expect("small sender-floor wake delivery");
    assert_eq!(sender_floor_wake.sequence, 4);
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        registry,
        factory,
        None,
        clock as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("deliver small sender-floor wake");
    assert_eq!(report.enqueued, 1);
    let queued_sequences = target
        .list_queued_work(target_session_id)
        .await
        .expect("list mixed-era receiver queue")
        .into_iter()
        .filter_map(|batch| {
            batch.items.into_iter().find_map(|item| match item.payload {
                crate::QueuedWorkPayload::ProcessWake { wake } if wake.process_id == process_id => {
                    Some(wake.sequence)
                }
                _ => None,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(queued_sequences, vec![1, 2, sender_floor_wake.sequence]);
}

async fn rewound_fresh_delivery_is_discarded_without_blocking(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    target: Arc<dyn crate::RuntimePersistence>,
    target_session_id: &str,
) {
    let process_id = "wake-store-rewind-poison";
    let old = crate::ProcessWakeDelivery {
        wake_id: "wake:store-rewind:10".to_string(),
        target_session_id: target_session_id.to_string(),
        process_id: process_id.to_string(),
        sequence: 10,
        event_type: "producer.wake".to_string(),
        event_invocation: crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new(target_session_id),
            "wake:store-rewind:10",
            crate::RuntimeEffectKind::Process,
            "wake:store-rewind:10",
        ),
        process_caused_by: None,
        input: "receiver state surviving a sender-store rewind".to_string(),
        created_at_ms: 10,
    };
    let old_batch = target
        .enqueue_queued_work(crate::process_wake_batch_draft(old.clone()))
        .await
        .expect("seed receiver floor above restored sender state");
    settle_queued_batch(&target, target_session_id, &old_batch.batch_id).await;
    registry
        .register_process(
            process_registry::registration(process_id)
                .with_extra_event_types([
                    process_registry::plain_event_type("producer.progress"),
                    process_registry::wake_event_type("producer.wake"),
                ])
                .with_wake_session_id(Some(target_session_id.to_string())),
        )
        .await
        .expect("register restored sender process");
    let poison = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "rewound fresh wake"}),
            ),
        )
        .await
        .expect("append rewound fresh wake")
        .wake_delivery
        .expect("rewound fresh outbox row");
    assert_eq!(poison.sequence, 1);
    for value in 2..=10 {
        registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::new(
                    "producer.progress",
                    serde_json::json!({"value": value}),
                ),
            )
            .await
            .expect("advance restored sender beyond receiver floor");
    }
    let healthy = registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "healthy wake after rewind"}),
            ),
        )
        .await
        .expect("append healthy post-rewind wake")
        .wake_delivery
        .expect("healthy post-rewind outbox row");
    assert_eq!(healthy.sequence, old.sequence + 1);

    let poison_report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        Arc::clone(&factory),
        None,
        Arc::clone(&clock) as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("drive rewound poison wake");
    assert_eq!(poison_report.discarded_sequence_rewound, 1);
    assert_eq!(poison_report.retryable_failures, 0);
    let delivery_report = registry
        .wake_delivery_report()
        .await
        .expect("report rewound discard");
    assert_eq!(delivery_report.sequence_rewound, 1);
    assert!(
        delivery_report
            .blocked_groups
            .iter()
            .all(|group| group.process_id != process_id),
        "sequence-rewound discard must not block its ordering group"
    );

    let healthy_report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        factory,
        None,
        clock as Arc<dyn crate::Clock>,
        32,
    )
    .await
    .expect("drive healthy wake behind rewound discard");
    assert_eq!(healthy_report.enqueued, 1);
    assert!(
        target
            .list_queued_work(target_session_id)
            .await
            .expect("list healthy post-rewind queue")
            .iter()
            .any(|batch| {
                batch.source_key.as_deref()
                    == Some(crate::process_wake_source_key(process_id, healthy.sequence).as_str())
            })
    );
}

async fn target_gone_is_a_typed_discard(
    factory: Arc<dyn crate::SessionStoreFactory>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
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
        clock as Arc<dyn crate::Clock>,
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
    clock: Arc<TestClock>,
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
    clock.set(expires_at_ms);
    let report = crate::WakeDeliveryDriver::drive_pending_once(
        Arc::clone(&registry),
        factory,
        None,
        clock as Arc<dyn crate::Clock>,
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
