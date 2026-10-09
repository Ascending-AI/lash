//! Language producer identity has the same semantics on every session store.
use futures_util::StreamExt as _;
use lash_core::{
    LiveReplayEventDraft, LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore,
    LiveReplayStoreError, LiveReplaySubscribeOutcome, SessionObservationEventPayload,
    SessionRevision,
};
use lash_sansio::SessionId;
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "conformance fixture: a refused operation fails the named law"
)]
pub(super) async fn language_identity_is_window_scoped_and_conflicts_retire_continuity(
    store: Arc<dyn LiveReplayStore>,
) {
    let session = SessionId::from("language-identity");
    let revision = SessionRevision::new(0);
    let start = store.current_cursor(&session, revision);
    let original =
        lash_core::testing::session_language_observation(&session, "cell-start", "first");
    let draft = |observation| {
        LiveReplayEventDraft::new(
            None::<lash_sansio::TurnId>,
            SessionObservationEventPayload::LanguageExecution(observation),
        )
    };
    let events = store
        .publish(&session, revision, vec![draft(original.clone())])
        .await
        .expect("first publication");
    assert_eq!(events.len(), 1);
    let SessionObservationEventPayload::LanguageExecution(read) = &events[0].payload else {
        panic!("language payload survives the store");
    };
    assert_eq!(read, &original);
    let last = events[0].cursor.clone();
    let mut redelivered = original.clone();
    redelivered.observed_at_ms = 100;
    assert!(
        store
            .publish(&session, revision, vec![draft(redelivered)])
            .await
            .expect("clock-independent retry")
            .is_empty()
    );
    assert_eq!(store.current_cursor(&session, revision), last);
    let LiveReplaySubscribeOutcome::Subscribed(mut live) = store
        .subscribe_after_cursor(&last)
        .await
        .expect("subscribe")
    else {
        panic!("continuous replay");
    };
    let conflict =
        lash_core::testing::session_language_observation(&session, "cell-start", "different fact");
    assert!(
        matches!(store.publish(&session, revision, vec![draft(conflict.clone())]).await, Err(LiveReplayStoreError::ConflictingLanguageRedelivery { session_id, event_key }) if session_id == session && event_key == "cell-start")
    );
    assert!(matches!(
        live.next().await,
        Some(Err(LiveReplayStoreError::Closed))
    ));
    assert!(matches!(
        store
            .replay_after_cursor(&start)
            .await
            .expect("read retired window"),
        LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable)
    ));
    let fresh = store.current_cursor(&session, revision);
    let events = store
        .publish(&session, revision, vec![draft(conflict.clone())])
        .await
        .expect("identity is released with window");
    assert_eq!(events.len(), 1);
    assert_ne!(events[0].cursor, last);
    let LiveReplayOutcome::Replayed(events) = store
        .replay_after_cursor(&fresh)
        .await
        .expect("fresh window")
    else {
        panic!("new continuity");
    };
    assert_eq!(events.len(), 1);
    let SessionObservationEventPayload::LanguageExecution(read) = &events[0].payload else {
        panic!("typed language replay");
    };
    assert_eq!(read, &conflict);
}
