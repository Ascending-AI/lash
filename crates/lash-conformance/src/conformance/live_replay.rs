//! [`LiveReplayStore`] conformance: the generic replay laws for the session
//! store, and what only session payloads can state.
//!
//! These vectors are the store-only portion of the ratified live-replay law
//! family. Laws which need the authoritative projection belong at the public
//! runtime seam rather than in a host-store fitness contract.

use super::replay_laws::{
    ReplayLawEvent, ReplayLawGap, ReplayLawKind, ReplayLawOutcome,
    replay_incarnation_change_invalidates_cursor, replay_store_burst, replay_store_capacity_trim,
    replay_store_laws, replay_store_ttl_trim,
};
use super::*;
use crate::runtime::LiveReplayEventDraft;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use pretty_assertions::assert_eq;

/// The session live replay store, as the generic replay laws drive it.
pub struct SessionReplayLaws;
#[path = "live_replay/language.rs"]
mod language;

#[async_trait::async_trait]
impl ReplayLawKind for SessionReplayLaws {
    type Store = dyn LiveReplayStore;
    type Subject = SessionId;
    type Cursor = crate::SessionCursor;
    type Event = Arc<SessionObservationEvent>;
    type Error = LiveReplayStoreError;
    type Subscription = crate::LiveReplaySubscription;

    fn subject(label: &str) -> SessionId {
        SessionId::fixture(label.to_string())
    }

    async fn publish(
        store: &Self::Store,
        subject: &SessionId,
        revision: u64,
        labels: Vec<String>,
    ) -> Result<Vec<Self::Event>, LiveReplayStoreError> {
        let drafts = labels
            .iter()
            .map(|label| LiveReplayEventDraft::new(None::<TurnId>, live_replay_text_payload(label)))
            .collect();
        let events = store
            .publish(subject, SessionRevision::new(revision), drafts)
            .await?;
        for event in &events {
            assert_event_readers_match_cursor(event, subject);
        }
        Ok(events)
    }

    async fn current_cursor(
        store: &Self::Store,
        subject: &SessionId,
        revision: u64,
    ) -> crate::SessionCursor {
        store.current_cursor(subject, SessionRevision::new(revision))
    }

    async fn replay(
        store: &Self::Store,
        cursor: &crate::SessionCursor,
    ) -> Result<ReplayLawOutcome<Vec<Self::Event>>, LiveReplayStoreError> {
        Ok(match store.replay_after_cursor(cursor).await? {
            LiveReplayOutcome::Replayed(events) => ReplayLawOutcome::Continued(events),
            LiveReplayOutcome::Gap(reason) => ReplayLawOutcome::Gap(law_gap(reason)),
        })
    }

    async fn subscribe(
        store: &Self::Store,
        cursor: &crate::SessionCursor,
    ) -> Result<ReplayLawOutcome<Self::Subscription>, LiveReplayStoreError> {
        Ok(match store.subscribe_after_cursor(cursor).await? {
            LiveReplaySubscribeOutcome::Subscribed(subscription) => {
                ReplayLawOutcome::Continued(subscription)
            }
            LiveReplaySubscribeOutcome::Gap(reason) => ReplayLawOutcome::Gap(law_gap(reason)),
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot invalidate fails the law"
    )]
    async fn invalidate(store: &Self::Store, subject: &SessionId) {
        store
            .invalidate_session(subject)
            .await
            .expect("invalidate a session");
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot trim fails the law"
    )]
    async fn trim(store: &Self::Store, subject: &SessionId) {
        store.trim_session(subject).await.expect("trim a session");
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store writes cursors in the parsable form"
    )]
    fn describe(event: &Self::Event) -> ReplayLawEvent<crate::SessionCursor> {
        let parsed = event.cursor.parse().expect("a published cursor parses");
        ReplayLawEvent {
            cursor: event.cursor.clone(),
            label: live_replay_event_label(event),
            position: parsed.live_position,
            revision: parsed.revision.as_u64(),
            incarnation: parsed.replay_incarnation_id.to_string(),
        }
    }

    fn cursor_at(
        incarnation: &str,
        subject: &SessionId,
        revision: u64,
        position: u64,
    ) -> crate::SessionCursor {
        crate::SessionCursor::new(
            incarnation,
            subject,
            SessionRevision::new(revision),
            position,
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the cursor's serde form is transparent"
    )]
    fn malformed_cursor() -> crate::SessionCursor {
        serde_json::from_value(serde_json::json!("not-a-session-cursor"))
            .expect("construct malformed cursor through public serde surface")
    }

    fn is_malformed_cursor_error(error: &LiveReplayStoreError) -> bool {
        matches!(
            error,
            LiveReplayStoreError::Cursor(crate::SessionCursorError::Malformed { .. })
        )
    }

    fn is_closed(error: &LiveReplayStoreError) -> bool {
        matches!(error, LiveReplayStoreError::Closed)
    }
}

#[async_trait::async_trait]
impl super::replay_laws::ReplayLawInvalidateAll for SessionReplayLaws {
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture: failure to invalidate is a law failure"
    )]
    async fn invalidate_all(store: &Self::Store) {
        store
            .invalidate_all()
            .await
            .expect("invalidate every session window");
    }
}

fn law_gap(reason: LiveReplayGapReason) -> ReplayLawGap {
    match reason {
        LiveReplayGapReason::Trimmed => ReplayLawGap::Trimmed,
        LiveReplayGapReason::Unavailable => ReplayLawGap::Unavailable,
    }
}

/// `make` must return a fresh, empty store on each call.
///
/// The generic replay laws ([`replay_store_laws`]), then the session
/// store's own: every payload kind and turn id survives the store, and a
/// redrive of streamed deltas, framed alike or not, adds no text twice and
/// loses none silently.
pub async fn live_replay_store<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    replay_store_laws::<SessionReplayLaws, _>(&make).await;
    super::replay_laws::store_wide_invalidation_gaps_every_subscriber::<SessionReplayLaws>(make())
        .await;
    session_payloads_and_turns_survive_the_store(make()).await;
    language::language_identity_is_window_scoped_and_conflicts_retire_continuity(make()).await;
    a_redrive_adds_no_streamed_text_twice_and_loses_none(make()).await;
}

/// See [`replay_store_burst`].
pub async fn live_replay_store_burst<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    replay_store_burst::<SessionReplayLaws, _>(make).await;
}

/// Together with [`live_replay_store_ttl_trim`], this states the store-owned
/// portion of `capacity_and_age_trim_force_snapshot`
/// ([`replay_store_capacity_trim`]).
pub async fn live_replay_store_capacity_trim<F>(make: F)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    replay_store_capacity_trim::<SessionReplayLaws, _>(make).await;
}

/// See [`replay_store_ttl_trim`].
pub async fn live_replay_store_ttl_trim<F>(make: F, expiration_wait: Duration)
where
    F: Fn() -> Arc<dyn LiveReplayStore>,
{
    replay_store_ttl_trim::<SessionReplayLaws, _>(make, expiration_wait).await;
}

/// Law 9 ([`replay_incarnation_change_invalidates_cursor`]).
pub async fn incarnation_change_invalidates_cursor(
    original: Arc<dyn LiveReplayStore>,
    fresh: Arc<dyn LiveReplayStore>,
    preserved: Arc<dyn LiveReplayStore>,
) {
    replay_incarnation_change_invalidates_cursor::<SessionReplayLaws>(original, fresh, preserved)
        .await;
}

/// An event comes back from the store as it went in: its session, its
/// revision, its turn id or the absence of one, and its payload, whichever
/// kind it is.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_payloads_and_turns_survive_the_store(store: Arc<dyn LiveReplayStore>) {
    let session = SessionId::from("payload-session");
    let revision = SessionRevision::new(7);
    let start = store.current_cursor(&session, revision);
    let text = publish_one(
        &store,
        &session,
        revision,
        Some(&TurnId::from("alpha-turn")),
        live_replay_text_payload("alpha one"),
    )
    .await
    .expect("append a turn activity");
    let process = publish_one(
        &store,
        &session,
        revision,
        None,
        SessionObservationEventPayload::ProcessChanged {
            kind: SessionProcessEventKind::Started { sequence: 1 },
            process_ids: vec![crate::ProcessId::fixture("proc-b")],
        },
    )
    .await
    .expect("append a process change");
    let queue = publish_one(
        &store,
        &session,
        SessionRevision::new(8),
        None,
        SessionObservationEventPayload::QueueChanged {
            kind: SessionQueueEventKind::Enqueued,
            batch_ids: vec!["batch-a".to_string()],
        },
    )
    .await
    .expect("append a queue change");

    assert_eq!(text.session_id(), "payload-session");
    assert_eq!(text.revision(), revision);
    assert_eq!(text.turn_id.as_deref(), Some("alpha-turn"));
    assert_eq!(process.turn_id, None);
    assert_eq!(queue.turn_id, None);
    assert_eq!(queue.revision(), SessionRevision::new(8));

    let LiveReplayOutcome::Replayed(replayed) = store
        .replay_after_cursor(&start)
        .await
        .expect("replay the session")
    else {
        panic!("a replay from the session's first cursor continues");
    };
    let started = format!(
        "process:Started {{ sequence: 1 }}:{}",
        crate::ProcessId::fixture("proc-b")
    );
    assert_eq!(
        replayed
            .iter()
            .map(|event| live_replay_event_label(event))
            .collect::<Vec<_>>(),
        ["alpha one", started.as_str(), "queue:Enqueued:batch-a"]
    );
    assert_eq!(replayed[0].turn_id.as_deref(), Some("alpha-turn"));
    assert_eq!(replayed[1].turn_id, None);
}

/// A redrive republishes the activities its first attempt delivered under
/// the ids the same observations derive: `{key}#{ordinal}` for one delta,
/// `{key}#{first}..{last}` for a frame of them (FIG-5098). It may frame them
/// differently. A store drops every redelivery inside what it delivered of
/// that key, so no text lands twice; publishes what lies beyond it, so none
/// is lost; and answers a frame straddling its edge, whose undelivered text
/// cannot be cut from its delivered text, with a gap rather than either.
async fn a_redrive_adds_no_streamed_text_twice_and_loses_none(store: Arc<dyn LiveReplayStore>) {
    let session = SessionId::from("framed-redrive");
    let revision = SessionRevision::new(1);
    let start = store.current_cursor(&session, revision);

    assert_eq!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#0", "a"), ("k#1..3", "bcd")]
        )
        .await,
        vec!["a", "bcd"]
    );
    assert!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#0", "a"), ("k#1", "b"), ("k#2", "c"), ("k#3", "d")]
        )
        .await
        .is_empty(),
        "the unmerged originals of a delivered frame are redeliveries"
    );
    assert!(
        deliver_framed(&store, &session, revision, &[("k#1..2", "bc")])
            .await
            .is_empty(),
        "a different framing inside the delivered range is a redelivery"
    );
    assert_eq!(
        deliver_framed(
            &store,
            &session,
            revision,
            &[("k#3", "d"), ("k#4..5", "ef")]
        )
        .await,
        vec!["ef"],
        "what lies beyond the delivered range is published"
    );
    assert!(
        matches!(
            store.replay_after_cursor(&start).await,
            Ok(LiveReplayOutcome::Replayed(replayed))
                if replayed.iter().map(|event| live_replay_event_label(event)).collect::<Vec<_>>()
                    == ["a", "bcd", "ef"]
        ),
        "the replay holds each streamed delta once"
    );

    assert!(
        deliver_framed(&store, &session, revision, &[("k#5..7", "fgh")])
            .await
            .is_empty(),
        "a frame straddling the delivered range is not published"
    );
    assert!(
        matches!(
            store.replay_after_cursor(&start).await,
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ),
        "a replay across a straddling redelivery is a gap"
    );
}

/// Publish `drafts` as framed text deltas, answering the published labels.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deliver_framed(
    store: &Arc<dyn LiveReplayStore>,
    session: &SessionId,
    revision: SessionRevision,
    drafts: &[(&str, &str)],
) -> Vec<String> {
    let drafts = drafts
        .iter()
        .map(|(id, text)| LiveReplayEventDraft::new(None::<TurnId>, framed_text_payload(id, text)))
        .collect();
    store
        .publish(session, revision, drafts)
        .await
        .expect("publish streamed deltas")
        .iter()
        .map(|event| live_replay_event_label(event))
        .collect()
}

fn framed_text_payload(id: &str, text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity {
        id: crate::TurnActivityId::new(id),
        correlation_id: crate::TurnActivityId::new("text:0"),
        event: TurnEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        }),
    })
}

fn live_replay_text_payload(text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity::independent(TurnEvent::StreamBlock(
        StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            text: text.into(),
            block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    )))
}

async fn publish_one(
    store: &Arc<dyn LiveReplayStore>,
    session_id: &SessionId,
    revision: SessionRevision,
    turn_id: Option<&TurnId>,
    payload: SessionObservationEventPayload,
) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
    let event = store
        .publish(
            session_id,
            revision,
            vec![LiveReplayEventDraft::new(turn_id, payload)],
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| LiveReplayStoreError::Store("published batch was empty".to_string()))?;
    assert_event_readers_match_cursor(&event, session_id);
    Ok(event)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn assert_event_readers_match_cursor(
    event: &SessionObservationEvent,
    expected_session_id: &SessionId,
) {
    let parsed = event
        .cursor
        .parse_for_session(expected_session_id)
        .expect("every emitted live replay event must carry a valid cursor for its session");
    assert_eq!(event.session_id(), parsed.session_id);
    assert_eq!(event.replay_incarnation_id(), parsed.replay_incarnation_id);
    assert_eq!(event.revision(), parsed.revision);
}

fn live_replay_event_label(event: &SessionObservationEvent) -> String {
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            }) => text.to_string(),
            other => format!("turn:{other:?}"),
        },
        SessionObservationEventPayload::Committed { .. } => "committed".to_string(),
        SessionObservationEventPayload::LanguageExecution(observation) => {
            observation.execution.event_key.clone()
        }
        SessionObservationEventPayload::ResidentChanged => "resident_changed".to_string(),
        SessionObservationEventPayload::AgentFrameSwitched { frame_id, .. } => {
            format!("frame:{frame_id}")
        }
        SessionObservationEventPayload::QueueChanged { kind, batch_ids } => {
            format!("queue:{kind:?}:{}", batch_ids.join(","))
        }
        SessionObservationEventPayload::ProcessChanged { kind, process_ids } => {
            format!("process:{kind:?}:{}", process_ids.join(","))
        }
    }
}
