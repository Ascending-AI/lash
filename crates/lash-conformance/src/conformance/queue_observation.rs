//! A failed post-mutation head read makes observation continuity unavailable.

use super::*;
use futures_util::StreamExt as _;
use lash_sansio::SessionId;

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn queue_head_read_failure_publishes_recoverable_gap(backend: crate::Backend) {
    for committed in [false, true] {
        let session_id = SessionId::from(format!("queue-head-gap-{committed}"));
        let factory = backend.session_store_factory();
        let view = factory
            .admit_view(&crate::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: crate::SessionRelation::Root,
                config: crate::SessionPolicy::new(crate::TurnBudget::Unbounded).into(),
                head: crate::SessionCreationHead::CommittedByCreator,
            })
            .await
            .expect("create queue session");
        if committed {
            let state = RuntimeSessionState {
                session_id: session_id.clone(),
                ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
            };
            view.commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
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
        let LiveReplaySubscribeOutcome::Subscribed(mut subscription) =
            replay.subscribe_after_cursor(&cursor).expect("subscribe")
        else {
            panic!("healthy cursor subscribes");
        };
        let ops = crate::facade_support::DurableSessionOps::new(
            session_id.clone(),
            lash_core::drive::IngressRelay::over_backend(
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
            replay.replay_after_cursor(&cursor),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ));
        assert!(matches!(
            replay.subscribe_after_cursor(&cursor),
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
            matches!(replay.replay_after_cursor(&recovered), Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty())
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
