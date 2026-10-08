//! Laws of a follower whose live replay runs behind its run: a gap costs it
//! only what the replay lost (FIG-5486), and a run it reads ended from the
//! store answers at once with everything the run published, because a turn's
//! activity reaches the replay before its commit is durable (FIG-5507).

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

/// A live replay store that fails its node at a chosen point of a turn's
/// publication: it holds the turn's model call record, and so everything its
/// node publishes after it, until released (`hold_record`), or refuses every
/// `Committed` observation, as a node that died right after its commit
/// publishes none (`refuse_commits`).
#[derive(Debug)]
struct FaultyReplay {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    hold_record: bool,
    /// Publications reaching the held record.
    held: AtomicUsize,
    release: tokio::sync::Semaphore,
    refuse_commits: bool,
    /// `Committed` publications refused.
    refused: AtomicUsize,
}

impl FaultyReplay {
    fn new(hold_record: bool, refuse_commits: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: Default::default(),
            hold_record,
            held: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            refuse_commits,
            refused: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for FaultyReplay {
    async fn publish(
        &self,
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        if self.hold_record
            && events.iter().any(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::TurnActivity(activity)
                        if matches!(activity.event, crate::TurnEvent::ModelCallRecorded { .. })
                )
            })
        {
            self.held.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("the release is never closed")
                .forget();
        }
        if self.refuse_commits
            && events.iter().any(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::Committed { .. }
                )
            })
        {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(lash_core::LiveReplayStoreError::Store(
                "the committing node died before it published its commit".into(),
            ));
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

/// A core over a fresh SQLite memory store set whose node publishes to
/// `replay`.
fn core_publishing_to(backend: lash_core::Backend, replay: Arc<FaultyReplay>) -> Result<LashCore> {
    LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .serve_test_llm_profile(
            scripted_provider(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))),
            mock_llm_profile_spec(),
        )
        .live_replay_store(replay)
        .build(crate::testing::runtime_lease_owner())
}

/// The answer of a follower that watched its run: the run's model call
/// among its activity and in its report, and no gap.
fn assert_whole(watched: &crate::SendOutcome) {
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
}

/// A node that dies right after a turn's commit publishes no `Committed`
/// for it. The run's follower learns that it ended from the store, where the
/// terminal is, and answers the committed result at once, whole and with no
/// gap: it waits for no observation of the commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_answers_a_run_whose_node_died_after_its_commit_at_once() -> Result<()> {
    let replay = FaultyReplay::new(false, true);
    let core = core_publishing_to(sqlite_memory_store_backend().await, replay.clone())?;
    let session = core
        .session(
            crate::SessionId::parse("send-unpublished-commit").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text("watch me"))
        .id(crate::TurnId::parse("unpublished-commit-run").expect("nonblank host identity"))
        .await?;
    let watched = tokio::time::timeout(std::time::Duration::from_secs(3), handle.outcome())
        .await
        .expect("the follower waited for a commit observation its run's node never published")?;
    assert!(
        replay.refused.load(Ordering::SeqCst) > 0,
        "the commit's observation was refused"
    );
    assert_whole(&watched);
    Ok(())
}

/// A turn's activity reaches the live replay before its commit is durable,
/// so a reader that finds the run ended in the store finds everything the
/// run published already there. While the replay holds the run's model call
/// record unpublished, the run has no terminal; once it is published the
/// run's follower answers with it and with no gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runs_terminal_is_durable_only_once_its_activity_is_published() -> Result<()> {
    let replay = FaultyReplay::new(true, false);
    let core = core_publishing_to(sqlite_memory_store_backend().await, replay.clone())?;
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
    let watching = tokio::spawn(handle.outcome());
    reaches(&replay.held, 1, "the model call record is held").await;
    // A follower that attaches now reads the run's end from the store alone.
    let mut settled = tokio::spawn(session.attach(input_id).outcome());
    let early = tokio::time::timeout(std::time::Duration::from_millis(500), &mut settled).await;
    assert!(
        early.is_err(),
        "the run's terminal was durable while its model call was still unpublished"
    );
    replay.release.add_permits(1);
    let settled = settled.await.expect("the attached follower")?;
    assert_eq!(settled.status(), crate::TurnStatus::Answered);
    assert_whole(&watching.await.expect("the watching follower")?);
    Ok(())
}
