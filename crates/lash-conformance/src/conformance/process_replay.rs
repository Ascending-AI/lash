//! [`ProcessReplayStore`] conformance: the generic replay laws for the
//! process store, and what only process observations can state.

use std::sync::atomic::{AtomicU64, Ordering};

use super::replay_laws::{
    ReplayLawEvent, ReplayLawGap, ReplayLawInvalidateAll, ReplayLawKind, ReplayLawOutcome,
    invalidation_gaps_the_subject_alone, replay_incarnation_change_invalidates_cursor,
    replay_store_burst, replay_store_capacity_trim, replay_store_laws, replay_store_ttl_trim,
    store_wide_invalidation_gaps_every_subscriber,
};
use super::*;
use crate::testing::{
    process_committed_event, process_language_observation, process_observation_label,
};
use crate::{
    ProcessId, ProcessObservationCursor, ProcessObservationCursorError, ProcessObservationEvent,
    ProcessObservationIdentity, ProcessReplayEventDraft, ProcessReplayGapReason,
    ProcessReplayOutcome, ProcessReplayStore, ProcessReplayStoreError,
    ProcessReplaySubscribeOutcome, ProcessReplaySubscription, ProcessSequence,
};
use pretty_assertions::assert_eq;

/// The process replay store, as the generic replay laws drive it.
pub struct ProcessReplayLaws;

/// Makes every law publication a new observation: the store drops a
/// repeated event key as a redelivery.
static NEXT_EVENT_KEY: AtomicU64 = AtomicU64::new(0);

#[async_trait::async_trait]
impl ReplayLawKind for ProcessReplayLaws {
    type Store = dyn ProcessReplayStore;
    type Subject = ProcessId;
    type Cursor = ProcessObservationCursor;
    type Event = Arc<ProcessObservationEvent>;
    type Error = ProcessReplayStoreError;
    type Subscription = ProcessReplaySubscription;

    fn subject(label: &str) -> ProcessId {
        ProcessId::fixture(label)
    }

    async fn publish(
        store: &Self::Store,
        subject: &ProcessId,
        revision: u64,
        labels: Vec<String>,
    ) -> Result<Vec<Self::Event>, ProcessReplayStoreError> {
        let drafts = labels
            .iter()
            .map(|label| {
                let key = NEXT_EVENT_KEY.fetch_add(1, Ordering::Relaxed);
                ProcessReplayEventDraft::language_execution(
                    ProcessSequence::new(revision),
                    process_language_observation(subject, &format!("law:{key}"), label),
                )
            })
            .collect();
        let events = store.publish(subject, drafts).await?;
        for event in &events {
            assert_eq!(&event.process_id(), subject, "an event names its process");
        }
        Ok(events)
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot name a cursor fails the law"
    )]
    async fn current_cursor(
        store: &Self::Store,
        subject: &ProcessId,
        revision: u64,
    ) -> ProcessObservationCursor {
        store
            .current_cursor(subject, ProcessSequence::new(revision))
            .await
            .expect("the process's current cursor")
    }

    async fn replay(
        store: &Self::Store,
        cursor: &ProcessObservationCursor,
    ) -> Result<ReplayLawOutcome<Vec<Self::Event>>, ProcessReplayStoreError> {
        Ok(match store.replay_after_cursor(cursor).await? {
            ProcessReplayOutcome::Replayed(events) => ReplayLawOutcome::Continued(events),
            ProcessReplayOutcome::Gap(reason) => ReplayLawOutcome::Gap(law_gap(reason)),
        })
    }

    async fn subscribe(
        store: &Self::Store,
        cursor: &ProcessObservationCursor,
    ) -> Result<ReplayLawOutcome<Self::Subscription>, ProcessReplayStoreError> {
        Ok(match store.subscribe_after_cursor(cursor).await? {
            ProcessReplaySubscribeOutcome::Subscribed(subscription) => {
                ReplayLawOutcome::Continued(subscription)
            }
            ProcessReplaySubscribeOutcome::Gap(reason) => ReplayLawOutcome::Gap(law_gap(reason)),
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot invalidate fails the law"
    )]
    async fn invalidate(store: &Self::Store, subject: &ProcessId) {
        store
            .invalidate_process(subject)
            .await
            .expect("invalidate a process");
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot trim fails the law"
    )]
    async fn trim(store: &Self::Store, subject: &ProcessId) {
        store.trim_process(subject).await.expect("trim a process");
    }

    fn describe(event: &Self::Event) -> ReplayLawEvent<ProcessObservationCursor> {
        ReplayLawEvent {
            cursor: event.cursor.clone(),
            label: process_observation_label(event),
            position: event.live_position(),
            revision: event.sequence().as_u64(),
            incarnation: event.replay_incarnation_id().to_string(),
        }
    }

    fn cursor_at(
        incarnation: &str,
        subject: &ProcessId,
        revision: u64,
        position: u64,
    ) -> ProcessObservationCursor {
        ProcessObservationCursor::new(
            incarnation,
            subject,
            ProcessSequence::new(revision),
            position,
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the cursor's serde form is transparent"
    )]
    fn malformed_cursor() -> ProcessObservationCursor {
        serde_json::from_value(serde_json::json!("not-a-process-cursor"))
            .expect("construct malformed cursor through public serde surface")
    }

    fn is_malformed_cursor_error(error: &ProcessReplayStoreError) -> bool {
        matches!(
            error,
            ProcessReplayStoreError::Cursor(ProcessObservationCursorError::Malformed { .. })
        )
    }

    fn is_closed(error: &ProcessReplayStoreError) -> bool {
        matches!(error, ProcessReplayStoreError::Closed)
    }
}

#[async_trait::async_trait]
impl ReplayLawInvalidateAll for ProcessReplayLaws {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a store that cannot invalidate fails the law"
    )]
    async fn invalidate_all(store: &Self::Store) {
        store
            .invalidate_all()
            .await
            .expect("invalidate every process");
    }
}

fn law_gap(reason: ProcessReplayGapReason) -> ReplayLawGap {
    match reason {
        ProcessReplayGapReason::Trimmed => ReplayLawGap::Trimmed,
        ProcessReplayGapReason::Unavailable => ReplayLawGap::Unavailable,
    }
}

/// `make` must return a fresh, empty store on each call.
///
/// The generic replay laws ([`replay_store_laws`]) and both invalidations,
/// then the process store's own: a redelivered observation or committed
/// fact is published once, and one redelivered with a different fact is
/// never applied.
pub async fn process_replay_store<F>(make: F)
where
    F: Fn() -> Arc<dyn ProcessReplayStore>,
{
    replay_store_laws::<ProcessReplayLaws, _>(&make).await;
    invalidation_gaps_the_subject_alone::<ProcessReplayLaws>(make()).await;
    store_wide_invalidation_gaps_every_subscriber::<ProcessReplayLaws>(make()).await;
    a_redelivery_is_published_once(make()).await;
    a_conflicting_redelivery_is_a_gap(make()).await;
}

/// See [`replay_store_burst`].
pub async fn process_replay_store_burst<F>(make: F)
where
    F: Fn() -> Arc<dyn ProcessReplayStore>,
{
    replay_store_burst::<ProcessReplayLaws, _>(make).await;
}

/// See [`replay_store_capacity_trim`].
pub async fn process_replay_store_capacity_trim<F>(make: F)
where
    F: Fn() -> Arc<dyn ProcessReplayStore>,
{
    replay_store_capacity_trim::<ProcessReplayLaws, _>(make).await;
}

/// See [`replay_store_ttl_trim`].
pub async fn process_replay_store_ttl_trim<F>(make: F, expiration_wait: Duration)
where
    F: Fn() -> Arc<dyn ProcessReplayStore>,
{
    replay_store_ttl_trim::<ProcessReplayLaws, _>(make, expiration_wait).await;
}

/// See [`replay_incarnation_change_invalidates_cursor`].
pub async fn process_replay_incarnation_change_invalidates_cursor(
    original: Arc<dyn ProcessReplayStore>,
    fresh: Arc<dyn ProcessReplayStore>,
    preserved: Arc<dyn ProcessReplayStore>,
) {
    replay_incarnation_change_invalidates_cursor::<ProcessReplayLaws>(original, fresh, preserved)
        .await;
}

async fn labels_after(
    store: &Arc<dyn ProcessReplayStore>,
    cursor: &ProcessObservationCursor,
) -> Vec<String> {
    match store.replay_after_cursor(cursor).await {
        Ok(ProcessReplayOutcome::Replayed(events)) => events
            .iter()
            .map(|event| process_observation_label(event))
            .collect(),
        other => panic!("expected a replay, got {other:?}"),
    }
}

/// A publication retry and a takeover's republication repeat what the
/// window holds: the same event key or the same committed sequence, stating
/// the same fact. The store publishes each once, whichever batch or writer
/// repeats it, and a later observation time does not make a new fact.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_redelivery_is_published_once(store: Arc<dyn ProcessReplayStore>) {
    let process = ProcessId::fixture("redelivery");
    let start = store
        .current_cursor(&process, ProcessSequence::new(0))
        .await
        .expect("a start cursor");
    let node = || process_language_observation(&process, "node:a:0:started", "node a");
    let drafts = || {
        vec![
            ProcessReplayEventDraft::language_execution(ProcessSequence::new(0), node()),
            ProcessReplayEventDraft::committed(process_committed_event(1, "operator")),
        ]
    };
    let published = store
        .publish(&process, drafts())
        .await
        .expect("publish an observation and a commit");
    assert_eq!(published.len(), 2);
    assert_eq!(published[1].sequence(), ProcessSequence::new(1));
    assert_eq!(
        published[1].payload.identity(),
        ProcessObservationIdentity::Committed {
            sequence: ProcessSequence::new(1)
        }
    );

    assert!(
        store
            .publish(&process, drafts())
            .await
            .expect("republish the batch")
            .is_empty(),
        "a republished batch publishes nothing"
    );
    let mut later = node();
    later.observed_at_ms = 99;
    let mixed = store
        .publish(
            &process,
            vec![
                ProcessReplayEventDraft::language_execution(ProcessSequence::new(1), later),
                ProcessReplayEventDraft::committed(process_committed_event(2, "operator")),
                ProcessReplayEventDraft::committed(process_committed_event(2, "operator")),
            ],
        )
        .await
        .expect("publish a batch that repeats and extends");
    assert_eq!(
        mixed
            .iter()
            .map(|event| process_observation_label(event))
            .collect::<Vec<_>>(),
        ["committed:2"],
        "only what the window does not hold is published, once"
    );
    assert_eq!(
        labels_after(&store, &start).await,
        ["node a", "committed:1", "committed:2"]
    );
}

/// The same identity with a different fact is not a redelivery and cannot
/// be applied beside the first: the batch fails with the typed conflict,
/// none of it is published, and the process's continuity is gone, so no
/// observer keeps either version as if nothing happened.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_conflicting_redelivery_is_a_gap(store: Arc<dyn ProcessReplayStore>) {
    for (name, first, conflicting, identity) in [
        (
            "conflict-committed",
            ProcessReplayEventDraft::committed(process_committed_event(1, "operator")),
            ProcessReplayEventDraft::committed(process_committed_event(1, "someone else")),
            ProcessObservationIdentity::Committed {
                sequence: ProcessSequence::new(1),
            },
        ),
        (
            "conflict-language",
            ProcessReplayEventDraft::language_execution(
                ProcessSequence::new(0),
                process_language_observation(&ProcessId::fixture("conflict-language"), "k", "one"),
            ),
            ProcessReplayEventDraft::language_execution(
                ProcessSequence::new(0),
                process_language_observation(&ProcessId::fixture("conflict-language"), "k", "two"),
            ),
            ProcessObservationIdentity::LanguageExecution {
                event_key: "k".to_string(),
            },
        ),
    ] {
        let process = ProcessId::fixture(name);
        let start = store
            .current_cursor(&process, ProcessSequence::new(0))
            .await
            .expect("a start cursor");
        store
            .publish(&process, vec![first])
            .await
            .expect("publish the first version");
        let unrelated = ProcessReplayEventDraft::language_execution(
            ProcessSequence::new(0),
            process_language_observation(&process, "unrelated", "unrelated"),
        );
        let error = store
            .publish(&process, vec![unrelated, conflicting])
            .await
            .expect_err("a conflicting redelivery fails its batch");
        assert!(
            matches!(
                &error,
                ProcessReplayStoreError::ConflictingRedelivery { process_id, identity: conflict }
                    if *process_id == process && *conflict == identity
            ),
            "{name}: expected the typed conflict, got {error}"
        );
        assert!(
            matches!(
                store.replay_after_cursor(&start).await,
                Ok(ProcessReplayOutcome::Gap(
                    ProcessReplayGapReason::Unavailable
                ))
            ),
            "{name}: a cursor across a conflict is a gap"
        );
        let fresh = store
            .current_cursor(&process, ProcessSequence::new(0))
            .await
            .expect("a cursor after the conflict");
        assert!(
            labels_after(&store, &fresh).await.is_empty(),
            "{name}: nothing of the conflicting batch was published"
        );
    }
}
