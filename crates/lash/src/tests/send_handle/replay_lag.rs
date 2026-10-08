//! Laws of a follower whose live replay runs behind its run (FIG-5486): a
//! gap costs it only what the replay lost, and a run it reads settled from
//! the store answers with the activity its node was still publishing.

use super::*;

/// The events the replay retains after `cursor`.
async fn retained(
    core: &LashCore,
    cursor: &lash_core::SessionCursor,
) -> Vec<Arc<lash_core::SessionObservationEvent>> {
    match core
        .live_replay_store
        .replay_after_cursor(cursor)
        .await
        .expect("replay after the handle's cursor")
    {
        lash_core::LiveReplayOutcome::Replayed(events) => events,
        lash_core::LiveReplayOutcome::Gap(reason) => {
            panic!("the handle's cursor is inside the window: {reason:?}")
        }
    }
}

/// A stream that restarts with a gap (a re-sent model call whose earlier
/// attempts the replay cannot prove, FIG-5399) goes on publishing at once.
/// A follower that meets the gap later resumes from the start of what the
/// replay retains, so it reads what the restart published before it
/// resubscribed, where resuming at the replay's head skipped it: the
/// completed attempt of a resumed call among it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_past_a_restarted_stream_reads_what_the_restart_published() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-restarted-stream").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("restarted-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    // The run's own activity names its physical turn and revision.
    let started = retained(&fixture.core, handle.cursor())
        .await
        .into_iter()
        .find(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(_)
            )
        })
        .expect("the held run published its start");
    let lash_core::SessionObservationEventPayload::TurnActivity(activity) = &started.payload else {
        unreachable!("selected above")
    };
    let after_restart = lash_core::TurnActivityId::new("published-after-the-restart");
    let store = &fixture.core.live_replay_store;
    store
        .invalidate_session(&session.session_id())
        .await
        .expect("restart the stream with a gap");
    store
        .publish(
            &session.session_id(),
            started.revision(),
            vec![lash_core::LiveReplayEventDraft::new(
                started.turn_id.clone(),
                lash_core::SessionObservationEventPayload::TurnActivity(lash_core::TurnActivity {
                    id: after_restart.clone(),
                    correlation_id: after_restart.clone(),
                    event: activity.event.clone(),
                }),
            )],
        )
        .await
        .expect("publish after the restart");

    let mut events = handle.events();
    let wait = std::time::Duration::from_secs(2);
    let gap = tokio::time::timeout(wait, events.next())
        .await
        .expect("the stream reports its gap")
        .expect("gap item");
    assert!(
        matches!(gap, Err(EmbedError::Send(error)) if matches!(*error, crate::SendError::ObservationGap(_)))
    );
    let next = tokio::time::timeout(wait, events.next()).await;
    assert!(
        matches!(&next, Ok(Some(Ok(activity))) if activity.id == after_restart),
        "the follower skipped what the restarted stream published before it resubscribed: {next:?}"
    );
    fixture.release.notify_one();
    while let Some(item) = tokio::time::timeout(std::time::Duration::from_secs(20), events.next())
        .await
        .expect("the stream ends once the run settles")
    {
        item.expect("the run's activity");
    }
    Ok(())
}

/// A live replay store that holds a turn's model call record, and so
/// everything its node publishes after it, until released: a shared store's
/// publication running behind the turn's commit.
#[derive(Debug)]
struct LaggingReplay {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    /// Publications reaching the held record.
    held: AtomicUsize,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for LaggingReplay {
    async fn publish(
        &self,
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        if events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(activity)
                    if matches!(activity.event, crate::TurnEvent::ModelCallRecorded { .. })
            )
        }) {
            self.held.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("the release is never closed")
                .forget();
        }
        self.inner.publish(session, revision, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session, revision)
    }

    fn earliest_cursor(
        &self,
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.earliest_cursor(session, revision)
    }

    async fn invalidate_session(
        &self,
        session: &lash_core::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session).await
    }

    async fn trim_session(
        &self,
        session: &lash_core::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session).await
    }
}

/// A turn's commit is durable before its node has published the turn's last
/// activity. A follower that watched the run and reads it settled from the
/// store lets the replay reach the commit before it answers, so its report
/// carries the run's model call, where answering at the store read dropped
/// the record still being published, with no gap to say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_that_reads_its_run_settled_waits_for_the_activity_still_being_published()
-> Result<()> {
    let replay = Arc::new(LaggingReplay {
        inner: Default::default(),
        held: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(sqlite_memory_store_backend().await)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .live_replay_store(replay.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("send-lagging-replay").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text("watch me"))
        .id(crate::TurnId::parse("lagging-run").expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    let mut watching = tokio::spawn(handle.outcome());
    // A follower that attaches at the replay's head once publication is
    // held sees none of the run, so it answers as soon as the store shows
    // the run settled.
    reaches(&replay.held, 1, "the model call record is held").await;
    let settled = session.attach(input_id).outcome().await?;
    assert_eq!(settled.status(), crate::TurnStatus::Answered);

    let early = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut watching).await;
    assert!(
        early.is_err(),
        "the follower answered while its run's model call was still unpublished: {early:?}"
    );
    replay.release.add_permits(1);
    let watched = watching.await.expect("the watching follower")?;
    assert_eq!(watched.status(), crate::TurnStatus::Answered);
    assert!(watched.gaps().is_empty(), "{:?}", watched.gaps());
    let output = watched.output().expect("an answered run has a report");
    assert!(
        output
            .activities
            .iter()
            .any(|activity| matches!(activity.event, crate::TurnEvent::ModelCallRecorded { .. })),
        "{:?}",
        output.activities
    );
    assert_eq!(output.result.llm_calls.len(), 1);
    Ok(())
}
