//! A failed post-mutation head read makes observation continuity unavailable.

use super::*;
use futures_util::StreamExt as _;
use lash_sansio::SessionId;

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn queue_head_read_failure_publishes_recoverable_gap(backend: crate::Backend) {
    for committed in [false, true] {
        let session_id = SessionId::fixture(format!("queue-head-gap-{committed}"));
        let factory = backend.session_store_factory();
        let view = factory
            .admit_view(&crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                config: crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
                .into(),
                head: crate::SessionCreationHead::Config,
            })
            .await
            .expect("create queue session");
        if committed {
            let state = RuntimeSessionState {
                session_id: session_id.clone(),
                ..RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ))
            };
            view.commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
                .await
                .expect("commit a nonzero head");
        }
        let head = view.load_session_head_meta().await.expect("read real head");
        let revision = head
            .as_ref()
            .filter(|head| head.checkpoint_ref.is_some())
            .map_or(SessionRevision::new(0), |head| {
                SessionRevision::new(head.head_revision)
            });
        if committed {
            assert!(revision > SessionRevision::new(0));
        }
        let recording = Arc::new(crate::testing::runtime_helpers::RecordingStore::over(
            view.store().clone(),
        ));
        let store = crate::store::SessionStore::new(recording.clone(), session_id.clone())
            .expect("valid view");
        let replay = Arc::new(crate::facade_support::InMemoryLiveReplayStore::default());
        let cursor = replay.current_cursor(&session_id, revision);
        let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = replay
            .subscribe_after_cursor(&cursor)
            .await
            .expect("subscribe")
        else {
            panic!("healthy cursor subscribes");
        };
        let ops = crate::facade_support::DurableSessionOps::new(
            session_id.clone(),
            lash_core::shift::IngressRelay::over_backend(
                &backend,
                Arc::new(crate::NoSessionWork::new()),
                backend.clock(),
            ),
            replay.clone(),
        );
        let first = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                session_id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("cancel after head read fails"),
            ))
            .await
            .expect("enqueue input");
        recording.fail_next_load_session_head_meta();
        assert!(
            ops.cancel_pending_turn_input(&store, first.input_id.as_str())
                .await
                .expect("mutation remains successful")
                .is_cancelled()
        );
        assert!(
            store
                .list_pending_turn_inputs()
                .await
                .expect("read durable queue")
                .is_empty()
        );
        assert_eq!(
            format!(
                "{:?}",
                store
                    .load_session_head_meta()
                    .await
                    .expect("head unchanged")
            ),
            format!("{head:?}")
        );
        assert_eq!(
            recording.load_session_head_meta_count(),
            2,
            "the injected read was reached"
        );
        assert!(matches!(
            replay.replay_after_cursor(&cursor).await,
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ));
        assert!(matches!(
            replay.subscribe_after_cursor(&cursor).await,
            Ok(LiveReplaySubscribeOutcome::Gap(
                LiveReplayGapReason::Unavailable
            ))
        ));
        assert!(
            matches!(
                subscription.next().await,
                Some(Err(LiveReplayStoreError::Closed))
            ),
            "an active observer is notified without a fabricated cursor"
        );
        let recovered = replay.current_cursor(&session_id, revision);
        assert_eq!(
            recovered
                .parse_for_session(&session_id)
                .expect("snapshot cursor parses")
                .revision,
            revision
        );
        assert!(
            matches!(replay.replay_after_cursor(&recovered).await, Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty())
        );
        let second = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                session_id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("healthy publication"),
            ))
            .await
            .expect("enqueue next input");
        ops.cancel_pending_turn_input(&store, second.input_id.as_str())
            .await
            .expect("healthy cancellation");
        let LiveReplayOutcome::Replayed(events) = replay
            .replay_after_cursor(&recovered)
            .await
            .expect("replay after resync")
        else {
            panic!("resynced cursor replays");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].revision(), revision);
        assert!(
            matches!(&events[0].payload, SessionObservationEventPayload::QueueChanged {
            kind: SessionQueueEventKind::Cancelled, batch_ids,
        } if batch_ids == &[second.input_id.to_string()])
        );
    }
}

struct FailingQueuePublication {
    inner: crate::facade_support::InMemoryLiveReplayStore,
    attempts: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl crate::LiveReplayStore for FailingQueuePublication {
    async fn publish(
        &self,
        _session: &SessionId,
        _revision: SessionRevision,
        events: Vec<crate::LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<crate::SessionObservationEvent>>, crate::LiveReplayStoreError> {
        assert!(events.iter().all(|event| matches!(
            event.payload,
            SessionObservationEventPayload::QueueChanged { .. }
        )));
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(crate::LiveReplayStoreError::Store(
            "injected queue publication failure".into(),
        ))
    }
    async fn replay_after_cursor(
        &self,
        cursor: &crate::SessionCursor,
    ) -> Result<LiveReplayOutcome, crate::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }
    async fn subscribe_after_cursor(
        &self,
        cursor: &crate::SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, crate::LiveReplayStoreError> {
        self.inner.subscribe_after_cursor(cursor).await
    }
    fn current_cursor(
        &self,
        session: &SessionId,
        revision: SessionRevision,
    ) -> crate::SessionCursor {
        self.inner.current_cursor(session, revision)
    }
    async fn trim_session(&self, session: &SessionId) -> Result<(), crate::LiveReplayStoreError> {
        self.inner.trim_session(session).await
    }
    async fn invalidate_session(
        &self,
        session: &SessionId,
    ) -> Result<(), crate::LiveReplayStoreError> {
        self.inner.invalidate_session(session).await
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn queue_publication_failure_preserves_committed_mutation(backend: crate::Backend) {
    {
        let id = SessionId::fixture("publication-failure");
        let store = backend
            .session_store_factory()
            .admit_view(&crate::testing::store_fixtures::session_store_request(
                &id,
                "queue-model",
                crate::SessionRelation::Root,
            ))
            .await
            .expect("admit queue owner");
        let input = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("durable mutation"),
            ))
            .await
            .expect("accept input");
        let replay = Arc::new(FailingQueuePublication {
            inner: Default::default(),
            attempts: Default::default(),
        });
        let ops = crate::facade_support::DurableSessionOps::new(
            id.clone(),
            lash_core::shift::IngressRelay::over_backend(
                &backend,
                Arc::new(crate::NoSessionWork::new()),
                backend.clock(),
            ),
            replay.clone(),
        );
        assert!(
            ops.cancel_pending_turn_input(&store, input.input_id.as_str())
                .await
                .expect("publication failure must not fail committed cancel")
                .is_cancelled()
        );
        assert_eq!(
            replay.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the injected boundary was reached"
        );
        assert!(
            store
                .list_pending_turn_inputs()
                .await
                .expect("durable queue after publication error")
                .is_empty()
        );
        assert!(
            !ops.cancel_pending_turn_input(&store, input.input_id.as_str())
                .await
                .expect("replayed cancellation")
                .is_cancelled()
        );
        assert_eq!(
            replay.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no event for an absent queue item"
        );
    }
}

#[derive(Default)]
struct CountingQueueDriver(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl crate::SessionWorkEngine for CountingQueueDriver {
    fn schedule_shift(&self, _session: &SessionId, _request: crate::engine::ShiftRequestId) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn install_session_shifts(
        &self,
        shifts: Arc<dyn crate::SessionShifts>,
    ) -> Arc<dyn crate::SessionShifts> {
        shifts
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn absent_or_deleted_durable_operations_emit_no_driver_wake<F, Fut>(
    backend: crate::Backend,
    exercise: F,
) where
    F: Fn(crate::Backend, SessionId) -> Fut,
    Fut: std::future::Future<Output = (bool, bool, bool)>,
{
    let shifts = Arc::new(CountingQueueDriver::default());
    let backend = crate::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(shifts.clone())
        .into_backend();
    let factory = backend.session_store_factory();
    for deleted in [false, true] {
        let id = SessionId::fixture(format!("no-wake-{deleted}"));
        if deleted {
            factory
                .admit_session(&crate::testing::store_fixtures::session_store_request(
                    &id,
                    "queue-model",
                    crate::SessionRelation::Root,
                ))
                .await
                .expect("admit then delete");
            factory
                .delete_session(&id)
                .await
                .expect("delete durable session");
        }
        assert_eq!(
            exercise(backend.clone(), id.clone()).await,
            (false, false, false)
        );
        assert_eq!(shifts.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(matches!(
            factory
                .lookup_session(&id)
                .await
                .expect("operations did not create"),
            crate::SessionLookup::Absent | crate::SessionLookup::Deleted
        ));
    }
    let id = SessionId::from("wake-positive-control");
    factory
        .admit_session(&crate::testing::store_fixtures::session_store_request(
            &id,
            "queue-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("admit positive control");
    assert_eq!(exercise(backend, id).await, (true, true, true));
    assert_eq!(
        shifts.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the witness sees real `SessionShifts` asks"
    );
}
