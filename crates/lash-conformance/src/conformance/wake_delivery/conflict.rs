use super::*;
use lash_core::testing::ProcessRegistryFaults;
use pretty_assertions::assert_eq;
use std::sync::atomic::{AtomicBool, Ordering};

/// Backend-owned read of a receiver's wake redelivery floor, which no store
/// API exposes: the law asserts the floor a conflict raised while the
/// receiver's own wake is still open, before any other terminal could.
#[async_trait::async_trait]
pub trait WakeRedeliveryFloorProbe: Send + Sync {
    async fn receiver_floor(&self, session_id: &SessionId, process_id: &ProcessId) -> Option<u64>;
}

/// A deployment whose next process-wake enqueue fails with a transient
/// store fault before it reaches the receiver's transaction.
struct WakeEnqueueFaults {
    inner: Arc<dyn crate::DeploymentStore>,
    fail_next: AtomicBool,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for WakeEnqueueFaults {
    type Inner = dyn crate::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        draft: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::StoreError> {
        if draft.process_wake_source.is_some() && self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(crate::StoreError::Contended);
        }
        self.inner.enqueue_queued_work_with_outcome(draft).await
    }
}

impl crate::DeploymentStoreDecorator for WakeEnqueueFaults {}

fn wake_input(batch: &crate::QueuedWorkBatch) -> Option<&str> {
    match &batch.payload {
        crate::QueuedWorkPayload::ProcessWake { wake } => Some(wake.input.as_str()),
        crate::QueuedWorkPayload::SessionCommand { .. } => None,
    }
}

/// FIG-4487 (ADR 0101 §8, §9): a delivery whose process fact differs from
/// the wake the receiver already holds under the same process and sequence
/// ends as the typed, non-blocking `ContentConflict` discard. The receiver
/// keeps its own wake and raises its redelivery floor to the sequence in
/// the refusing transaction, before the sender can acknowledge the discard.
/// A transient fault before that commit stays retryable; a lost
/// acknowledgement after it converges on the same terminal. The conflict is
/// never retried, the later wake in the ordering group is admitted exactly
/// once, and after vacuum a retry at or below the floor is
/// `ProcessWakeSequenceRewound`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn conflicting_wake_delivery_is_terminal_and_later_delivery_progresses(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    clock: Arc<TestClock>,
    work: Arc<dyn crate::SessionWorkEngine>,
    floors: Arc<dyn WakeRedeliveryFloorProbe>,
) {
    let target_id = SessionId::from("wake-conflict-target");
    let target = factory
        .admit_view(&crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: target_id.clone(),
            relation: crate::SessionRelation::Root,
            config: crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
            .into(),
            head: crate::SessionCreationHead::CommittedByCreator,
        })
        .await
        .expect("create conflict target");
    let process_id = registry
        .register_process(
            process_registry::registration("wake-conflict-sender")
                .with_extra_event_types([process_registry::wake_event_type("producer.wake")])
                .with_wake_session_id(Some(target_id.clone())),
        )
        .await
        .expect("register conflict sender")
        .id;
    let mut deliveries = Vec::new();
    for input in ["changed content", "later wake"] {
        deliveries.push(
            registry
                .append_event(
                    &process_id,
                    crate::ProcessEventAppendRequest::new(
                        "producer.wake",
                        serde_json::json!({"wake_input": input}),
                    ),
                )
                .await
                .expect("append conflict-law wake")
                .wake_delivery
                .expect("durable wake delivery"),
        );
    }
    let later = deliveries.pop().expect("later delivery");
    let changed = deliveries.pop().expect("changed delivery");
    assert!(later.sequence > changed.sequence);

    // The receiver already holds a different wake under the changed
    // delivery's process and sequence: a restored sender reallocated it.
    let mut original = changed.clone();
    original.input = "the receiver's own wake".to_string();
    let original_draft = crate::process_wake_batch_draft(original.clone());
    let original_digest = original_draft
        .submission_digest()
        .expect("digest original wake");
    assert_ne!(
        crate::process_wake_batch_draft(changed.clone())
            .submission_digest()
            .expect("digest changed wake"),
        original_digest,
        "the changed delivery carries a different process fact"
    );
    let original_batch = target
        .store()
        .enqueue_queued_work(original_draft)
        .await
        .expect("seed the receiver's own wake");

    let receiver = Arc::new(WakeEnqueueFaults {
        inner: Arc::clone(&factory),
        fail_next: AtomicBool::new(false),
    });
    let sender = ProcessRegistryFaults::new(Arc::clone(&registry));
    let drive = || {
        crate::WakeDeliveryDriver::drive_pending_once(
            Arc::new(sender.clone()) as Arc<dyn crate::ProcessRegistry>,
            Arc::clone(&receiver) as Arc<dyn crate::DeploymentStore>,
            Arc::clone(&work),
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
            8,
        )
    };
    let delivery_state = |delivery_id: String| {
        let registry = Arc::clone(&registry);
        async move {
            registry
                .list_wake_deliveries(None)
                .await
                .expect("list wake deliveries")
                .into_iter()
                .find(|row| row.delivery_id == delivery_id)
                .expect("delivery row")
                .disposition
        }
    };
    let receiver_wakes = || async {
        let mut wakes = target
            .list_queued_work()
            .await
            .expect("read receiver queue")
            .into_iter()
            .filter(|batch| {
                batch.source_key.as_deref().is_some_and(|key| {
                    key == crate::process_wake_source_key(&process_id, changed.sequence)
                        || key == crate::process_wake_source_key(&process_id, later.sequence)
                })
            })
            .map(|batch| {
                (
                    batch.source_key.clone().expect("wake source key"),
                    wake_input(&batch).expect("wake payload").to_string(),
                    batch.submission_digest.clone(),
                )
            })
            .collect::<Vec<_>>();
        wakes.sort();
        wakes
    };
    let original_only = vec![(
        crate::process_wake_source_key(&process_id, changed.sequence),
        original.input.clone(),
        original_digest.clone(),
    )];

    // A transient fault before the receiver's transaction: nothing commits,
    // the delivery stays pending, and the later wake waits behind it.
    receiver.fail_next.store(true, Ordering::SeqCst);
    let transient = drive().await.expect("drive through a transient fault");
    assert_eq!(transient.inspected, 1, "{transient:?}");
    assert_eq!(transient.retryable_failures, 1, "{transient:?}");
    assert_eq!(transient.discarded_content_conflict, 0, "{transient:?}");
    assert_eq!(transient.enqueued, 0, "{transient:?}");
    assert_eq!(
        delivery_state(changed.wake_id.clone()).await,
        crate::WakeDeliveryLifecycle::Pending,
        "a transient store fault stays retryable"
    );
    assert_eq!(
        floors.receiver_floor(&target_id, &process_id).await,
        None,
        "a fault before the receiver's commit raises no floor"
    );
    assert_eq!(receiver_wakes().await, original_only);

    // The receiver refuses and commits its floor; the sender's
    // acknowledgement of the discard is lost.
    clock.advance(1_000);
    sender.fail_next_wake_discard(crate::PluginError::Session(
        "injected lost discard acknowledgement".to_string(),
    ));
    let unacknowledged = drive().await.expect("drive through a lost acknowledgement");
    assert_eq!(unacknowledged.inspected, 1, "{unacknowledged:?}");
    assert_eq!(unacknowledged.retryable_failures, 1, "{unacknowledged:?}");
    assert_eq!(
        unacknowledged.discarded_content_conflict, 0,
        "{unacknowledged:?}"
    );
    assert_eq!(
        delivery_state(changed.wake_id.clone()).await,
        crate::WakeDeliveryLifecycle::Pending,
        "the sender has not acknowledged the discard"
    );
    assert_eq!(
        floors.receiver_floor(&target_id, &process_id).await,
        Some(changed.sequence),
        "the receiver's floor commits with its refusal, before the sender acknowledges"
    );
    assert_eq!(receiver_wakes().await, original_only);

    // The retry converges on the same terminal: a typed conflict discard
    // that does not hold the ordering group.
    clock.advance(1_000);
    let discarded = drive().await.expect("drive the conflict to its terminal");
    assert_eq!(discarded.inspected, 1, "{discarded:?}");
    assert_eq!(discarded.discarded_content_conflict, 1, "{discarded:?}");
    assert_eq!(discarded.retryable_failures, 0, "{discarded:?}");
    assert_eq!(discarded.enqueued, 0, "{discarded:?}");
    assert_eq!(
        delivery_state(changed.wake_id.clone()).await,
        crate::WakeDeliveryLifecycle::Discarded {
            reason: crate::WakeDiscardReason::ContentConflict,
        }
    );
    assert!(!crate::WakeDiscardReason::ContentConflict.blocks_ordering_group());
    let report = registry
        .wake_delivery_report()
        .await
        .expect("report conflict discard");
    assert_eq!(report.content_conflict, 1, "{report:?}");
    assert!(
        report
            .blocked_groups
            .iter()
            .all(|group| group.process_id != process_id),
        "a conflict discard must not block its ordering group: {report:?}"
    );
    assert_eq!(
        floors.receiver_floor(&target_id, &process_id).await,
        Some(changed.sequence)
    );

    // The later wake is admitted exactly once, and the conflict is never
    // retried.
    let progressed = drive().await.expect("drive the later wake");
    assert_eq!(progressed.inspected, 1, "{progressed:?}");
    assert_eq!(progressed.enqueued, 1, "{progressed:?}");
    assert_eq!(progressed.floor_absorbed, 0, "{progressed:?}");
    assert_eq!(progressed.retryable_failures, 0, "{progressed:?}");
    assert_eq!(
        delivery_state(later.wake_id.clone()).await,
        crate::WakeDeliveryLifecycle::Enqueued
    );
    clock.advance(1_000);
    let quiet = drive().await.expect("drive a settled outbox");
    assert_eq!(quiet, crate::WakeDeliveryDriveReport::default());
    let later_draft = crate::process_wake_batch_draft(later.clone());
    let mut expected = vec![
        original_only[0].clone(),
        (
            crate::process_wake_source_key(&process_id, later.sequence),
            later.input.clone(),
            later_draft.submission_digest().expect("digest later wake"),
        ),
    ];
    expected.sort();
    assert_eq!(
        receiver_wakes().await,
        expected,
        "the receiver keeps its own wake and holds the later one once"
    );
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "conflicting_wake_delivery_is_terminal_and_later_delivery_progresses",
        "process_id": process_id,
        "conflict_sequence": changed.sequence,
        "later_sequence": later.sequence,
        "discarded_content_conflict": discarded.discarded_content_conflict,
        "later_enqueued": progressed.enqueued,
        "blocks_ordering_group": false,
    }));

    // After both wakes leave and vacuum removes their tombstones, a retry
    // at or below the floor is a typed rewind, never new work.
    super::settle_queued_batch(target.store(), &target_id, original_batch.batch_id.as_str()).await;
    let later_batch = target
        .list_queued_work()
        .await
        .expect("read receiver queue")
        .into_iter()
        .find(|batch| {
            batch.source_key.as_deref()
                == Some(crate::process_wake_source_key(&process_id, later.sequence).as_str())
        })
        .expect("later wake is queued");
    super::settle_queued_batch(target.store(), &target_id, later_batch.batch_id.as_str()).await;
    target
        .store()
        .vacuum(&target_id)
        .await
        .expect("vacuum receiver tombstones");
    for retry in [changed.clone(), original.clone(), later.clone()] {
        let error = target
            .store()
            .enqueue_queued_work(crate::process_wake_batch_draft(retry.clone()))
            .await
            .expect_err("a retry at or below the floor is refused after vacuum");
        assert!(
            matches!(
                &error,
                crate::StoreError::ProcessWakeSequenceRewound {
                    sequence,
                    allocation_floor,
                    ..
                } if *sequence == retry.sequence && *allocation_floor == later.sequence
            ),
            "{error:?}"
        );
    }
    assert!(
        target
            .list_queued_work()
            .await
            .expect("read vacuumed receiver queue")
            .is_empty(),
        "a refused retry recreates no work"
    );
}
