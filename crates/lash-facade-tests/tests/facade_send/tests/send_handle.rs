use super::*;
use lash_facade_tests::send::*;

use futures_util::StreamExt;
use tokio::sync::Notify;

mod arrival;
mod cross_node;
mod forsake;

/// FIG-6311: absence is provisional; a recorded refusal retains its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_probe_distinguishes_unknown_refused_and_accepted() -> Result<()> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&attempts);
    let plugin = lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("probe-refusal"),
        lash_core::facade_support::PluginSpec::new().with_after_turn(
            lash_core::hook_key!("probe-refusal"),
            Arc::new(move |_| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(lash_execution_contracts::plugin_error::PluginError::Invoke(
                            "probe refusal".to_owned(),
                        ));
                    }
                    Ok(Default::default())
                })
            }),
        ),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        scripted_provider(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))),
        mock_llm_profile_spec(),
    )
    .plugin(Arc::new(plugin))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::fixture("probe-input-outcome"),
    )
    .await
    .open()
    .await?;
    let id = lash_vocabulary::identity::TurnId::fixture("probe-before-send");
    for outcome in [
        session.attach_id(id.clone()).outcome().await?,
        session.durable().attach_id(id.clone()).outcome().await?,
    ] {
        assert_eq!(
            serde_json::to_value(&outcome)?["type"],
            "unknown",
            "an unseen id is not a final refusal: {outcome:?}"
        );
        assert!(outcome.run().is_none());
        assert!(outcome.refusal().is_none());
        assert!(outcome.output().is_none());
    }
    let sent = session
        .send(TurnInput::text("refused by the callback"))
        .id(id.clone())
        .await?;
    let refused = sent.outcome().await?;
    let reason = refused.refusal().expect("typed recorded refusal");
    assert!(reason.message.contains("probe refusal"), "{reason:?}");
    let attached = session.durable().attach_id(id).outcome().await?;
    assert!(matches!(attached, lash::SendOutcome::Refused { .. }));
    assert_eq!(
        serde_json::to_value(attached.refusal())?,
        serde_json::to_value(Some(reason))?
    );

    // Pre-admission errors leave the id reusable: the probe remains provisional.
    let valid_id = lash_vocabulary::identity::TurnId::fixture("probe-valid-send");
    let error = session
        .send(TurnInput::text("valid input"))
        .id(valid_id.clone())
        .model("no-such-model")
        .await
        .map(|_| ())
        .expect_err("unknown model refuses acceptance");
    assert!(matches!(error, EmbedError::Runtime(ref error)
        if error.code == lash_core::RuntimeErrorCode::LlmProfileUnknown));
    let probe = session.attach_id(valid_id.clone());
    assert!(probe.events().collect::<Vec<_>>().await.is_empty());
    session
        .send(TurnInput::text("valid input"))
        .id(valid_id)
        .await?
        .outcome()
        .await?;
    // The same handle must re-read after its events observed absence.
    assert_eq!(probe.outcome().await?.status(), lash::TurnStatus::Answered);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    core.shutdown().await?;
    Ok(())
}

/// FIG-6047: attaching to an absent or deleted session decides without waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_sessions_answer_unknown_promptly() -> Result<()> {
    let fixture = fixture(1).await?;
    let id = SessionId::fixture("absent-input-outcome");
    let missing = fixture.core.session(id.clone()).durable().await?;
    let key = lash_vocabulary::identity::TurnId::fixture("never-sent");
    let answer = |session: lash::DurableSession| {
        let key = key.clone();
        async move {
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                session.attach_id(key).outcome(),
            )
            .await
            .expect("absence decides promptly")
            .expect("absence is an outcome, not a read failure");
            assert!(matches!(outcome, lash::SendOutcome::Unknown { .. }));
        }
    };
    answer(missing.clone()).await;
    let created = fixture
        .core
        .session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    assert!(created.pending_turn_inputs().await?.is_empty());
    let administration = fixture.core.session_administration().await;
    LashCore::delete_session(administration.delete_context(id.as_str())?).await?;
    assert_eq!(
        fixture.core.await_session_deletion(&id).await?,
        lash::SessionDeleteCompletion::Deleted
    );
    answer(created).await;
    answer(missing).await;
    fixture.core.shutdown().await?;
    Ok(())
}

/// FIG-7045: a follower resolves a settled input or run from one keyed
/// snapshot of its facts, and nothing else the follow reads repeats it.
/// Before, a bound input's resolution read the input's status and then its
/// run's terminal, and an undecided run's added its session actor's park.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_follow_resolves_from_one_store_read() -> Result<()> {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Phase, Script};

    let reads = Script::new();
    let fixture = fixture_over(1, |backend| {
        LayeredBackend::over(backend)
            .map_session_store_factory(|inner| reads.wrap("follower", inner))
            .into_backend()
    })
    .await?;
    let session = created_session(&fixture.core, SessionId::fixture("one-follow-read"))
        .await
        .open()
        .await?;
    let handle = session.send(TurnInput::text("first")).await?;
    let input = handle.input_id().clone();
    handle.output().await?;
    let run = session
        .attach(input.clone())
        .run()
        .await?
        .expect("settled run");
    // Stop the engine so only the follower reads the store.
    fixture.core.shutdown().await?;
    let calls = || {
        reads
            .trace()
            .into_iter()
            .filter(|call| call.phase == Phase::Before)
            .map(|call| call.op.name())
            .collect::<Vec<_>>()
    };
    for (subject, durable) in [
        ("input", false),
        ("input", true),
        ("run", false),
        ("run", true),
    ] {
        let before = calls().len();
        let outcome = match (subject, durable) {
            ("input", false) => session.attach(input.clone()).outcome().await?,
            ("input", true) => session.durable().attach(input.clone()).outcome().await?,
            (_, false) => session.run(run.clone()).outcome().await?,
            (_, true) => session.durable().run(run.clone()).outcome().await?,
        };
        assert_eq!(outcome.status(), lash::TurnStatus::Answered);
        let made = calls().split_off(before);
        assert_eq!(
            made,
            ["follow_facts"],
            "a settled {subject} follow (durable: {durable}) resolves from one keyed snapshot"
        );
    }
    Ok(())
}

/// FOLLOW-COALESCE: equivalent live readers share an in-flight snapshot;
/// a settlement hinted after that snapshot still wakes both promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equivalent_followers_share_a_read_and_keep_a_racing_completion() -> Result<()> {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Script, StoreOp};

    let directory = tempfile::tempdir().expect("SQLite law directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            directory.path().join("follow.db"),
            lash_sqlite_store::SqliteSynchronous::Full,
        )
        .await
        .expect("SQLite follower law store"),
    );
    use lash_execution_contracts::backend::StoreSet as _;
    let wakes = stores.wake_sink();
    let reads = Script::new();
    let backend = lash_conformance::backend_over(stores);
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|inner| reads.wrap("shared-follower", inner))
        .into_backend();
    let read_store = backend.session_store_factory();
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let hour = std::time::Duration::from_secs(3600);
    let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .live_replay_store(replay.clone())
        .observer_pacing(lash::ObserverPacing {
            follow: lash::PollPacing::new(hour, hour).expect("valid pacing"),
            ..lash::ObserverPacing::standard()
        })
        .build(lash_facade_tests::runtime_lease_owner())?;
    let fixture = Fixture {
        core,
        release,
        calls,
    };
    let session = created_session(&fixture.core, SessionId::fixture("shared-follow"))
        .await
        .open()
        .await?;
    let run = lash_core::TurnId::from("shared-follow-run");
    let sent = session.send(TurnInput::text(HELD)).id(run.clone()).await?;
    provider_called(&fixture, 1).await;
    let input = sent.input_id().clone();
    let subject = lash_core::store::FollowSubject::Input(input);
    let actor = lash_core::durable_port::ActorKey::session("shared-follow").expect("session actor");
    let mut first_watch = wakes.observe(actor.clone(), None).await?;
    let mut second_watch = wakes.observe(actor.clone(), None).await?;
    let mut third_watch = wakes.observe(actor.clone(), None).await?;
    let store =
        lash_core::store::SessionStore::new(read_store, SessionId::fixture("shared-follow"))?;
    let snapshot = reads.on(StoreOp::follow_facts).after().pause();
    let transferred = reads.on(StoreOp::follow_facts).nth(2).after().pause();
    {
        let mut first = Box::pin(first_watch.read_follow_facts(&store, &subject, |_| {}));
        snapshot.reached_by(&mut first, 1).await;
        let mut second = Box::pin(second_watch.read_follow_facts(&store, &subject, |_| {}));
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(
            reads.calls(StoreOp::follow_facts),
            1,
            "equivalent existing waiters share one snapshot"
        );
        drop(first);
        snapshot.open_all();
        transferred.reached_by(&mut second, 1).await;
        let mut third = Box::pin(third_watch.read_follow_facts(&store, &subject, |_| {}));
        assert!(futures_util::poll!(&mut third).is_pending());
        assert_eq!(
            reads.calls(StoreOp::follow_facts),
            2,
            "a dropped initializer transfers one read to its waiter"
        );
        // A subscriber arriving after the snapshot began must read afresh,
        // even while older readers share that snapshot under the same hint.
        let mut late = wakes.observe(actor.clone(), None).await?;
        late.read_follow_facts(&store, &subject, |_| {}).await?;
        assert_eq!(
            reads.calls(StoreOp::follow_facts),
            3,
            "late subscribers never inherit an older snapshot"
        );
        fixture.release.notify_one();
        let completion = tokio::time::timeout(std::time::Duration::from_secs(5), sent.outcome())
            .await
            .expect("the worker completes while the older snapshot is held")?;
        assert_eq!(completion.status(), lash::TurnStatus::Answered);
        wakes.wake(&lash_core::durable_port::Woken {
            actor: actor.clone(),
            state: lash_core::durable_port::ActorState::Ready,
            owner: None,
        });
        transferred.open_all();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(second, third)
        })
        .await
        .expect("shared reads finish promptly")?;
    }
    for watch in [&mut second_watch, &mut third_watch] {
        tokio::time::timeout(std::time::Duration::from_secs(5), watch.changed())
            .await
            .expect("a completion after the snapshot must leave its wake pending");
        let (completed, _) = watch
            .committed_turn(&subject)
            .expect("the acknowledged completion survives later hints");
        assert_eq!(completed, run);
    }
    let before_fresh = reads.calls(StoreOp::follow_facts);
    let mut fresh = wakes.observe(actor, None).await?;
    assert!(
        fresh.committed_turn(&subject).is_none(),
        "a new subscriber cannot use an earlier handoff"
    );
    fresh.read_follow_facts(&store, &subject, |_| {}).await?;
    assert_eq!(
        reads.calls(StoreOp::follow_facts),
        before_fresh + 1,
        "handoffs never replace a new subscriber's fresh read"
    );
    reads
        .finish()
        .expect("the snapshot barriers were exercised");
    fixture.core.shutdown().await?;
    Ok(())
}

/// FIG-6017: settled answers read only run evidence. Session windows are
/// loaded only when the host explicitly asks for a committed-head view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_outcomes_do_not_load_session_windows() -> Result<()> {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Script, StoreOp};

    let reads = Script::new();
    let fixture = fixture_over(1, |backend| {
        LayeredBackend::over(backend)
            .map_session_store_factory(|inner| reads.wrap("outcome-reader", inner))
            .into_backend()
    })
    .await?;
    let session = created_session(&fixture.core, SessionId::fixture("result-only"))
        .await
        .open()
        .await?;
    let handle = session.send(TurnInput::text("first")).await?;
    let input = handle.input_id().clone();
    handle.output().await?;
    let run = session
        .attach(input.clone())
        .run()
        .await?
        .expect("settled run");
    session.send(TurnInput::text("later")).output().await?;
    // Stop the engine so its own activation reads cannot affect the count.
    fixture.core.shutdown().await?;
    let before = reads.calls(StoreOp::load_session_window);
    for outcome in [
        session.attach(input.clone()).outcome().await?,
        session.durable().attach(input).outcome().await?,
        session.run(run.clone()).outcome().await?,
        session.durable().run(run.clone()).outcome().await?,
    ] {
        assert_eq!(
            outcome
                .output()
                .and_then(|output| output.assistant_message()),
            Some("echo: first")
        );
    }
    assert_eq!(
        reads.calls(StoreOp::load_session_window),
        before,
        "reading a settled result loaded an unrequested session window"
    );
    let ended = session
        .durable()
        .read_run(&run)
        .await?
        .expect("terminal window");
    assert_eq!(
        ended.turn_index(),
        1,
        "the explicit run read is pinned to its terminal"
    );
    let view = session.durable().read().await?.expect("committed head");
    assert_eq!(
        view.turn_index(),
        2,
        "the ordinary read answers the committed head"
    );
    assert!(
        reads.calls(StoreOp::load_session_window) > before,
        "the explicit state read exercises the counting seam"
    );
    Ok(())
}

/// A settled run whose report no execution in this process can still deposit
/// answers from the store at once: the follower waits for a live report only
/// while a run here may still deposit one, never on a run that ran elsewhere
/// or whose report is gone (FIG-3843). Before, it waited out the 5 s
/// live-report grace.
async fn a_settled_run_no_execution_here_can_report_answers_at_once() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-no-grace")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let handle = session
        .send(TurnInput::text("report me once"))
        .id(lash_vocabulary::identity::TurnId::parse("no-grace-run")
            .expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    let live = handle.output().await?;
    assert_eq!(live.status(), lash::TurnStatus::Answered);

    // The live report is taken and no run of this session is under way here,
    // so nothing can deposit another: a handle attached now answers from the
    // store at once.
    let started = std::time::Instant::now();
    let durable = session.attach(input_id).output().await?;
    let waited = started.elapsed();
    assert_eq!(durable.assistant_message(), Some("echo: report me once"));
    assert!(
        waited < std::time::Duration::from_secs(2),
        "a run no execution here can report answers at once, not after the live-report grace: waited {waited:?}"
    );
    Ok(())
}

/// The store reads one input's followers make while its run binding is
/// awaited: everything a resolution of the input reads.
#[derive(Default)]
struct StatusReads {
    input: std::sync::Mutex<Option<lash_vocabulary::identity::InputId>>,
    reads: AtomicUsize,
}

impl StatusReads {
    fn watch(&self, input: &lash_vocabulary::identity::InputId) {
        *self.input.lock_recover() = Some(input.clone());
    }

    fn watches(&self, input: &lash_vocabulary::identity::InputId) -> bool {
        self.input.lock_recover().as_ref() == Some(input)
    }

    fn watching(&self) -> bool {
        self.input.lock_recover().is_some()
    }

    fn count(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

/// A deployment store counting [`StatusReads`].
struct CountedStatusReads {
    inner: Arc<dyn lash_execution_contracts::deployment::DeploymentStore>,
    reads: Arc<StatusReads>,
}

impl lash_core::DeploymentStoreDecorator for CountedStatusReads {}

#[async_trait]
impl lash_core::RuntimeStoreDecorator for CountedStatusReads {
    type Inner = dyn lash_execution_contracts::deployment::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn run_of_input(
        &self,
        session_id: &SessionId,
        input: &lash_vocabulary::identity::InputId,
    ) -> std::result::Result<Option<lash_vocabulary::identity::TurnId>, lash_core::StoreError> {
        if self.reads.watches(input) {
            self.reads.count();
        }
        self.inner.run_of_input(session_id, input).await
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input: &lash_vocabulary::identity::InputId,
    ) -> std::result::Result<Option<lash_core::PendingTurnInputRead>, lash_core::StoreError> {
        if self.reads.watches(input) {
            self.reads.count();
        }
        self.inner.pending_turn_input(session_id, input).await
    }

    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<lash_core::store::SessionFault>, lash_core::StoreError> {
        if self.reads.watching() {
            self.reads.count();
        }
        self.inner.session_fault(session_id).await
    }

    async fn follow_facts(
        &self,
        session_id: &SessionId,
        subject: &lash_core::store::FollowSubject,
    ) -> std::result::Result<lash_core::store::FollowFacts, lash_core::StoreError> {
        if subject
            .input()
            .is_some_and(|input| self.reads.watches(input))
        {
            self.reads.count();
        }
        self.inner.follow_facts(session_id, subject).await
    }
}

/// A follower with no resident runtime reads its unbound input's status at
/// the poll floor, one keyed read per tick, however much retained activity
/// its replay delivers first. A run that runs on another worker announces
/// its binding to no event this process sees, so the floor read is how the
/// follower learns its run (FIG-3981); the session's retained events wake
/// one read between them, not one each (FIG-5915). Before, each retained
/// commit and queue change cost the follower four reads (binding, pending
/// input, binding again, fault) beside a separate binding probe at the floor.
async fn an_unbound_inputs_durable_follower_reads_its_status_once_per_floor_tick() -> Result<()> {
    let reads = Arc::new(StatusReads::default());
    let counted = Arc::clone(&reads);
    let fixture = fixture_with_stores(
        1,
        Arc::new(move |inner| {
            Arc::new(CountedStatusReads {
                inner,
                reads: Arc::clone(&counted),
            }) as Arc<_>
        }),
    )
    .await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-floor-read")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    // Settled sends leave their queue changes and commits in the replay a
    // durable follower replays from the session's start.
    const RETAINED: usize = 16;
    for turn in 0..RETAINED {
        let outcome = session
            .send(TurnInput::text(format!("retained {turn}")))
            .await?
            .outcome()
            .await?;
        assert_eq!(outcome.status(), lash::TurnStatus::Answered);
    }
    // The held run keeps the next input queued, bound to no run.
    let held = session.send(TurnInput::text(HELD)).await?;
    provider_called(&fixture, RETAINED + 1).await;
    let queued = session
        .send(TurnInput::text("queued behind the held run"))
        .await?;
    reads.watch(queued.input_id());
    let following = tokio::spawn(
        session
            .durable()
            .attach(queued.input_id().clone())
            .outcome(),
    );
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let made = reads.reads.load(Ordering::SeqCst);

    fixture.release.notify_one();
    assert_eq!(held.outcome().await?.status(), lash::TurnStatus::Answered);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), following)
        .await
        .expect("the queued input's follower answers once its run executes")
        .expect("the follower task completes")?;
    assert_eq!(outcome.status(), lash::TurnStatus::Answered);

    // The floor is 25 ms: two seconds hold eighty ticks, beside the first
    // read and the one the replayed events wake.
    assert!(
        (20..=84).contains(&made),
        "the follower reads its status once per floor tick, and its replayed events wake one read: {made} reads"
    );
    Ok(())
}

/// An input withdrawn while it waits answers Cancelled with no output, and
/// its narrow form refuses as not settled (D1 §1.6, §1.7).
async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-withdrawn")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let waiting = session
        .send(TurnInput::text("withdraw me"))
        .id(lash_vocabulary::identity::TurnId::parse("withdrawn-run")
            .expect("nonblank host identity"))
        .await?;
    let input_id = waiting.input_id().clone();
    let receipt = waiting.cancel().await?;
    assert!(
        matches!(receipt, lash::CancelReceipt::Withdrawn { .. }),
        "a queued input is withdrawn: {receipt:?}"
    );
    fixture.release.notify_one();
    assert_eq!(
        running.output().await?.assistant_message(),
        Some(format!("echo: {HELD}").as_str())
    );

    let outcome = waiting.outcome().await?;
    assert!(
        matches!(outcome, lash::SendOutcome::Withdrawn { .. }),
        "a withdrawn input answers Withdrawn: {outcome:?}"
    );
    assert!(
        matches!(
            session
                .attach_id(
                    lash_vocabulary::identity::TurnId::parse("withdrawn-run")
                        .expect("nonblank host identity")
                )
                .outcome()
                .await?,
            lash::SendOutcome::Withdrawn { .. }
        ),
        "the withdrawal stays on record under its id"
    );
    assert_eq!(outcome.status(), lash::TurnStatus::Cancelled);
    assert!(
        outcome.output().is_none(),
        "no turn applied a withdrawn input"
    );
    assert_eq!(
        outcome.run().cloned(),
        None,
        "no run took a withdrawn input"
    );
    let refusal = session
        .attach(input_id.clone())
        .output()
        .await
        .expect_err("a withdrawn input has no settled turn");
    assert!(
        matches!(
            &refusal,
            EmbedError::Send(error) if matches!(
                error.as_ref(),
                lash::SendError::NotSettled { input_id: refused, status: lash::TurnStatus::Cancelled }
                    if *refused == input_id
            )
        ),
        "{refusal:?}"
    );
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "the withdrawn input never ran"
    );
    Ok(())
}

/// An input a shift answers inside another input's run resolves Answered
/// with that run's turn: the handle reads which run applied it, never
/// "my shift ran it", so it neither ceded nor refused (review of #2290,
/// MEDIUM-4).
async fn an_input_answered_inside_another_run_resolves_answered_with_that_run() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-shared-run")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id(lash_vocabulary::identity::TurnId::parse("second-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id(lash_vocabulary::identity::TurnId::parse("third-run").expect("nonblank host identity"))
        .await?;
    let third_input = third.input_id().clone();
    fixture.release.notify_one();
    running.output().await?;

    let second = second.output().await?;
    let third = third.outcome().await?;
    assert_eq!(third.status(), lash::TurnStatus::Answered);
    assert_eq!(
        third.run().cloned(),
        Some(lash_vocabulary::identity::RunId::parse("second-run").unwrap())
    );
    let third = third.output().expect("an answered input has a report");
    // One turn applied both inputs, each as its own user row (FIG-5288), so
    // both handles answer its one reply.
    assert_eq!(third.assistant_message(), Some("echo: third"), "{third:?}");
    assert_eq!(third.assistant_message(), second.assistant_message());
    assert_eq!(third.outcome, second.outcome);

    let applied_by = session
        .durable()
        .turn_input_applications()
        .await?
        .into_iter()
        .find(|application| application.input_id == third_input)
        .map(|application| application.turn_id)
        .expect("the third input was applied");
    assert_eq!(
        applied_by,
        lash_vocabulary::identity::TurnId::from("second-run")
    );
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        2,
        "one turn answered both"
    );
    Ok(())
}

/// A session the engine opens before any host committed it (a send through a
/// durable handle to a brand-new session) pins its protocol's per-session
/// options with its first commit, as a host's open does, so a later open
/// rematerializes it instead of refusing a recorded config with no RLM
/// channel.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_the_engine_opens_first_reopens_under_its_recorded_protocol() -> Result<()> {
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        lash::testing::TestProvider::builder()
            .kind("engine-first-open")
            .complete(|_| async {
                Ok(lash_core::LlmResponse {
                    parts: vec![lash::testing::execute_code(
                        "await control.finish(\"engine answered\");",
                    )],
                    ..Default::default()
                })
            })
            .build()
            .into_handle(),
        mock_llm_profile_spec(),
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let durable = core
        .session(
            lash_kernel_value::identity::SessionId::parse("engine-first")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id(lash_vocabulary::identity::TurnId::parse("engine-first-run")
            .expect("nonblank host identity"))
        .output()
        .await?;
    drop(durable);

    let session = core
        .session(
            lash_kernel_value::identity::SessionId::parse("engine-first")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    let again = session
        .send(TurnInput::text("and a host opens it after"))
        .id(lash_vocabulary::identity::TurnId::parse("host-after-run")
            .expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(again.status(), lash::TurnStatus::Answered);
    Ok(())
}

/// A cancel reaches the work past a frame switch: the switch answers its
/// send, and its follow-on runs as its own run (FIG-5232), which a host
/// cancels by the id the frame names; the follow-on answers Cancelled.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_reaches_a_run_past_its_frame_switch() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        {
            let calls = Arc::clone(&calls);
            lash::testing::TestProvider::builder()
                .kind("cancel-past-frame-switch")
                .complete(move |_| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            return Ok(lash_facade_tests::control_cell_response(
                                0,
                                (r#"await control.continueAs({ task: "wait to be cancelled" });"#)
                                    .to_string(),
                            ));
                        }
                        std::future::pending::<()>().await;
                        unreachable!("the follow-on turn's call is cancelled")
                    }
                })
                .build()
                .into_handle()
        },
        mock_llm_profile_spec(),
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::parse("cancel-past-switch")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let switched = session
        .send(TurnInput::text("switch frames, then wait"))
        .id(
            lash_vocabulary::identity::TurnId::parse("cancel-past-switch-run")
                .expect("nonblank host identity"),
        )
        .output()
        .await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.outcome);
    };
    let follow_on_run = lash_core::runtime::durable::session_mail::frame_task_run(frame_key);
    reaches(&calls, 2, "the follow-on turn calls the provider").await;

    let follow_on = session.attach_id(follow_on_run.clone());
    let receipt = follow_on.cancel().origin("send-handle-law").await?;
    assert!(
        matches!(&receipt, lash::CancelReceipt::Cancelled { run, .. } if run.as_str() == follow_on_run.as_str()),
        "the cancel reaches the follow-on run: {receipt:?}"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), follow_on.outcome())
        .await
        .expect("the cancelled run answers")?;
    assert_eq!(outcome.status(), lash::TurnStatus::Cancelled);
    Ok(())
}

/// A host re-attaches to its input with nothing but the id it sent under,
/// on a durable session with no resident state, and follows the input into
/// the run that answered it: a keyed input's id is derived from its session
/// and key.
async fn a_host_reattaches_by_its_id_alone() -> Result<()> {
    let fixture = fixture(4).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-attach-id")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id(lash_vocabulary::identity::TurnId::parse("second-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id(lash_vocabulary::identity::TurnId::parse("third-run").expect("nonblank host identity"))
        .await?;
    let durable = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-attach-id")
                .expect("nonblank host identity"),
        )
        .durable()
        .await?;
    let attached = durable.attach_id(
        lash_vocabulary::identity::TurnId::parse("third-run").expect("nonblank host identity"),
    );
    assert_eq!(attached.input_id(), third.input_id());
    assert_eq!(
        attached.id(),
        Some(&lash_vocabulary::identity::TurnId::from("third-run"))
    );
    fixture.release.notify_one();
    running.outcome().await?;
    second.outcome().await?;

    let outcome = attached.outcome().await?;
    assert_eq!(outcome.status(), lash::TurnStatus::Answered);
    let output = outcome.output().expect("an answered input has a report");
    assert!(
        output
            .assistant_message()
            .is_some_and(|reply| reply.contains("third")),
        "{output:?}"
    );
    assert_eq!(
        session
            .attach_id(
                lash_vocabulary::identity::TurnId::parse("third-run")
                    .expect("nonblank host identity")
            )
            .outcome()
            .await?
            .status(),
        lash::TurnStatus::Answered
    );
    Ok(())
}

/// A run this follower never observed live answers with a reported
/// Unavailable gap, so its (empty) activity list is not taken for the run's
/// history; a follower that watched it run reports none.
async fn an_unobserved_run_answers_with_a_reported_gap() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-unobserved-run")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let handle = session
        .send(TurnInput::text("watch me"))
        .id(lash_vocabulary::identity::TurnId::parse("watched-run")
            .expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    let watched = handle.outcome().await?;
    assert_eq!(watched.status(), lash::TurnStatus::Answered);
    assert!(watched.gaps().is_empty(), "{:?}", watched.gaps());
    assert!(
        !watched
            .output()
            .expect("an answered run has a report")
            .activities
            .is_empty()
    );

    let unobserved = session.attach(input_id).outcome().await?;
    assert_eq!(unobserved.status(), lash::TurnStatus::Answered);
    let output = unobserved.output().expect("an answered run has a report");
    assert!(output.activities.is_empty());
    assert!(
        matches!(
            unobserved.gaps(),
            [gap] if gap.reason == lash_core::LiveReplayGapReason::Unavailable
        ),
        "{:?}",
        unobserved.gaps()
    );
    Ok(())
}

/// Assert `refused` is the typed identity conflict a refused batch answers.
fn assert_identity_conflict(refused: std::result::Result<Vec<lash::SendHandle>, EmbedError>) {
    let Err(EmbedError::Runtime(error)) = &refused else {
        panic!(
            "expected a typed refusal, got {:?}",
            refused.map(|handles| handles.len())
        );
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::DurableIdentityConflict,
        "{error:?}"
    );
}

#[tokio::test]
async fn send_batch_refuses_reserved_source_keys_without_admitting_other_members() -> Result<()> {
    let fixture = fixture(1).await?;
    fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("reserved-batch")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("reserved-batch")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    for key in ["command:refresh_tool_catalog:foreign"] {
        let refused = session
            .send_batch([
                (
                    lash_vocabulary::identity::TurnId::parse("host:must-roll-back")
                        .expect("nonblank host identity"),
                    TurnInput::text("valid member"),
                ),
                (
                    lash_vocabulary::identity::TurnId::parse(key).expect("nonblank host identity"),
                    TurnInput::text("reserved member"),
                ),
            ])
            .await;
        let Err(EmbedError::Runtime(error)) = refused else {
            panic!("a mixed reserved-key batch must be refused");
        };
        assert_eq!(error.code.as_str(), "ingress_reserved_source_key");
        assert!(error.is_terminal());
        let recorded = serde_json::to_value(&error)?;
        assert_eq!(recorded["cause"]["source_key"], key);
        assert!(session.durable().pending_turn_inputs().await?.is_empty());
    }
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "a refused batch executes nothing"
    );
    Ok(())
}

/// One batch answers one handle per input, in request order, and each input
/// is answered by the run that applied it (FIG-3842). Resending the batch
/// answers the same inputs and runs nothing again. A batch naming an
/// accepted id with other content, or one id twice, accepts nothing.
async fn all_ingress_entries_preserve_receipts_caps_and_cancel_outcomes() -> Result<()> {
    let fixture = fixture_over_with_batching(
        lash::QueuedWorkBatchingConfig::new(1).with_max_turn_input_admission(1),
        |backend| backend,
    )
    .await?;
    fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-batch")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-batch")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    let batch = || {
        [
            (
                lash_vocabulary::identity::TurnId::parse("batch-a")
                    .expect("nonblank host identity"),
                TurnInput::text("first"),
            ),
            (
                lash_vocabulary::identity::TurnId::parse("batch-b")
                    .expect("nonblank host identity"),
                TurnInput::text("second"),
            ),
            (
                lash_vocabulary::identity::TurnId::parse("batch-c")
                    .expect("nonblank host identity"),
                TurnInput::text("third"),
            ),
        ]
    };

    let handles = session.send_batch(batch()).await?;
    assert_eq!(
        handles.iter().map(lash::SendHandle::id).collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c"]
            .map(|id| Some(lash_vocabulary::identity::TurnId::from(id)))
            .iter()
            .map(Option::as_ref)
            .collect::<Vec<_>>(),
        "one handle per input, in request order"
    );
    let input_ids = handles
        .iter()
        .map(|handle| handle.input_id().clone())
        .collect::<Vec<_>>();
    for handle in handles {
        let outcome = handle.outcome().await?;
        assert_eq!(outcome.status(), lash::TurnStatus::Answered);
        assert!(
            outcome.run().is_some() && outcome.output().is_some(),
            "{outcome:?}"
        );
    }
    let calls = fixture.calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 3,
        "the configured input cap executes one input per run"
    );

    let resent = session.send_batch(batch()).await?;
    assert_eq!(
        resent
            .iter()
            .map(|handle| handle.input_id().clone())
            .collect::<Vec<_>>(),
        input_ids,
        "a resent batch answers the inputs it accepted"
    );
    for handle in resent {
        assert_eq!(handle.outcome().await?.status(), lash::TurnStatus::Answered);
    }
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        calls,
        "nothing ran again"
    );

    assert_identity_conflict(
        session
            .send_batch([
                (
                    lash_vocabulary::identity::TurnId::parse("batch-d")
                        .expect("nonblank host identity"),
                    TurnInput::text("new"),
                ),
                (
                    lash_vocabulary::identity::TurnId::parse("batch-b")
                        .expect("nonblank host identity"),
                    TurnInput::text("changed"),
                ),
            ])
            .await,
    );
    assert_identity_conflict(
        session
            .send_batch([
                (
                    lash_vocabulary::identity::TurnId::parse("batch-e")
                        .expect("nonblank host identity"),
                    TurnInput::text("once"),
                ),
                (
                    lash_vocabulary::identity::TurnId::parse("batch-e")
                        .expect("nonblank host identity"),
                    TurnInput::text("twice"),
                ),
            ])
            .await,
    );
    for never in ["batch-d", "batch-e"] {
        let outcome = session
            .attach_id(
                lash_vocabulary::identity::TurnId::parse(never).expect("nonblank host identity"),
            )
            .outcome()
            .await?;
        assert_eq!(
            outcome.status(),
            lash::TurnStatus::Unknown,
            "`{never}` of a refused batch was never accepted"
        );
    }
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    // These are the current creating, resident, and store-only entry verbs.
    let created = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-entry-matrix")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let held = created
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("entry-held").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, calls + 1).await;
    let live = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-entry-matrix")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    let durable = fixture
        .core
        .session(
            lash_kernel_value::identity::SessionId::parse("send-entry-matrix")
                .expect("nonblank host identity"),
        )
        .durable()
        .await?;
    let entries = vec![
        live.send(TurnInput::text("resident input"))
            .id(lash_vocabulary::identity::TurnId::parse("entry-live")
                .expect("nonblank host identity"))
            .await?,
        durable
            .send(TurnInput::text("durable input"))
            .id(lash_vocabulary::identity::TurnId::parse("entry-durable")
                .expect("nonblank host identity"))
            .await?,
        live.durable()
            .send(TurnInput::text("resident durable input"))
            .id(
                lash_vocabulary::identity::TurnId::parse("entry-live-durable")
                    .expect("nonblank host identity"),
            )
            .await?,
    ];
    let snapshots = entries
        .iter()
        .map(|handle| handle.receipt().clone())
        .collect::<Vec<_>>();
    let cancelled = durable
        .send_batch([
            (
                lash_vocabulary::identity::TurnId::parse("entry-cancel-a")
                    .expect("nonblank host identity"),
                TurnInput::text("cancel a"),
            ),
            (
                lash_vocabulary::identity::TurnId::parse("entry-cancel-b")
                    .expect("nonblank host identity"),
                TurnInput::text("cancel b"),
            ),
        ])
        .await?;
    for handle in cancelled {
        assert!(matches!(
            handle.cancel().await?,
            lash::CancelReceipt::Withdrawn { .. }
        ));
        assert_eq!(
            handle.outcome().await?.status(),
            lash::TurnStatus::Cancelled
        );
    }
    let pending = durable.pending_turn_inputs().await?;
    assert_eq!(
        pending.len(),
        4,
        "held run and three uncancelled admissions remain"
    );
    for (entry, snapshot) in entries.iter().zip(&snapshots) {
        assert_eq!(
            entry.receipt(),
            snapshot,
            "receipt remains its acceptance snapshot while queued"
        );
        let row = pending
            .iter()
            .find(|row| row.input.input_id == entry.input_id())
            .expect("each entry durably admitted");
        assert_eq!(row.input.session_id, snapshot.session_id);
        assert_eq!(row.input.source_key, snapshot.source_key);
    }
    fixture.release.notify_one();
    assert_eq!(held.outcome().await?.status(), lash::TurnStatus::Answered);
    for (entry, snapshot) in entries.into_iter().zip(&snapshots) {
        let id = entry.id().expect("host id").clone();
        assert_eq!(entry.outcome().await?.status(), lash::TurnStatus::Answered);
        let retry = durable
            .send(TurnInput::text(match id.as_str() {
                "entry-live" => "resident input",
                "entry-durable" => "durable input",
                _ => "resident durable input",
            }))
            .id(id)
            .await?;
        assert_eq!(retry.receipt(), snapshot);
    }
    assert!(durable.pending_turn_inputs().await?.is_empty());
    Ok(())
}

async fn exact_host_root_settlement(host_id: &str) -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("exact-host-settlement")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let input = TurnInput::text("only once");
    let first = session
        .send(input.clone())
        .id(lash_vocabulary::identity::TurnId::fixture(
            host_id.to_string(),
        ))
        .await?;
    let input_id = first.input_id().clone();
    assert_eq!(
        first.outcome().await?.run().cloned(),
        Some(lash_vocabulary::identity::RunId::parse(host_id).unwrap())
    );
    let durable = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session
            .run(lash_vocabulary::identity::RunId::parse(host_id).unwrap())
            .outcome(),
    )
    .await
    .expect("the exact host run settles")?;
    assert_eq!(
        durable.run().cloned(),
        Some(lash_vocabulary::identity::RunId::parse(host_id).unwrap())
    );
    assert_eq!(durable.status(), lash::TurnStatus::Answered);
    let retry = session
        .send(input)
        .id(lash_vocabulary::identity::TurnId::fixture(
            host_id.to_string(),
        ))
        .await?;
    assert_eq!(retry.input_id(), &input_id);
    let retried = tokio::time::timeout(std::time::Duration::from_secs(2), retry.outcome())
        .await
        .expect("the settled host id answers its retry")?;
    assert_eq!(
        retried.run().cloned(),
        Some(lash_vocabulary::identity::RunId::parse(host_id).unwrap())
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(
            session
                .attach_id(lash_vocabulary::identity::TurnId::fixture(
                    host_id.to_string()
                ))
                .cancel()
                .await?,
            lash::CancelReceipt::UnknownOrRevoked
        ),
        "a settled run accepts no cancel"
    );
    Ok(())
}

macro_rules! exact_host_run_laws {
    ($name:ident, $host:literal) => {
        mod $name {
            use super::*;
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn settlement() -> Result<()> {
                exact_host_root_settlement($host).await
            }
        }
    };
}

mod exact_host_runs {
    use super::*;
    exact_host_run_laws!(plain, "job");
    exact_host_run_laws!(canonical_suffix, "job:agent-frame:1");
    exact_host_run_laws!(leading_zero_suffix, "job:agent-frame:01");
    exact_host_run_laws!(signed_suffix, "job:agent-frame:+1");
}
mod sqlite_memory {

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_settled_run_no_execution_here_can_report_answers_at_once() -> Result<()> {
        super::a_settled_run_no_execution_here_can_report_answers_at_once().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unbound_inputs_durable_follower_reads_its_status_once_per_floor_tick() -> Result<()>
    {
        super::an_unbound_inputs_durable_follower_reads_its_status_once_per_floor_tick().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn all_ingress_entries_preserve_receipts_caps_and_cancel_outcomes() -> Result<()> {
        super::all_ingress_entries_preserve_receipts_caps_and_cancel_outcomes().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_host_reattaches_by_its_id_alone() -> Result<()> {
        super::a_host_reattaches_by_its_id_alone().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unobserved_run_answers_with_a_reported_gap() -> Result<()> {
        super::an_unobserved_run_answers_with_a_reported_gap().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
        super::a_withdrawn_send_answers_cancelled_without_output().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_input_answered_inside_another_run_resolves_answered_with_that_run() -> Result<()> {
        super::an_input_answered_inside_another_run_resolves_answered_with_that_run().await
    }
}

/// FIG-6589: a session observer follows admitted runs, including a host id
/// whose spelling also looks like a later physical turn.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_discovers_exact_host_run_across_frame_switch() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core =
        explicit_ephemeral_facets(super::rlm_core_builder_over(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            {
                let calls = Arc::clone(&calls);
                lash::testing::TestProvider::builder()
                    .kind("exact-host-run-observation")
                    .complete(move |_| {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            Ok(lash_facade_tests::control_cell_response(0, (if call == 0 {
                            r#"await control.continueAs({ task: "finish in the next frame" });"#
                        } else {
                            r#"await control.finish("done");"#
                        }).to_string()))
                        }
                    })
                    .build()
                    .into_handle()
            },
            mock_llm_profile_spec(),
        )
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::fixture("exact-host-run-observation"),
    )
    .await
    .open()
    .await?;
    let (_, mut observations) = session.observe().follow(lash::FeedStart::Head).await?;
    let host_run = lash_vocabulary::identity::RunId::parse("job:agent-frame:1")?;
    let sent = session
        .send(TurnInput::text("switch frames"))
        .id(lash_vocabulary::identity::TurnId::fixture(
            host_run.as_str(),
        ))
        .await?;
    let switched = sent.output().await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.outcome
    else {
        panic!("the first run switches frame: {:?}", switched.outcome);
    };
    let next_run = lash_core::runtime::durable::session_mail::frame_task_run(frame_key);
    let expected = [
        host_run,
        lash_vocabulary::identity::RunId::parse(next_run.as_str())?,
    ];
    for expected_run in expected {
        let (turn, discovered) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let item = observations.next().await.expect("session observation")?;
                let lash::observe::SessionObservationStreamItem::Event(event) = item else {
                    panic!("a fresh observer retains continuity");
                };
                let lash::observe::SessionObservationEventPayload::TurnActivity(activity) =
                    &event.payload
                else {
                    continue;
                };
                if let lash::TurnEvent::TurnStarted { turn_id, .. } = &activity.event {
                    // Read the public wire contract so this law also runs before
                    // the identity field is introduced.
                    let value = serde_json::to_value(&activity.event)?;
                    let run = value["run_id"]
                        .as_str()
                        .expect("turn start names its admitted run");
                    break Result::Ok((
                        turn_id.clone(),
                        lash_vocabulary::identity::RunId::parse(run)?,
                    ));
                }
            }
        })
        .await
        .expect("each admitted run announces its start")?;
        assert_eq!(discovered, expected_run);
        assert_eq!(turn.as_str(), discovered.as_str());
        let followed = session.run(discovered.clone()).outcome().await?;
        assert_eq!(followed.run(), Some(&discovered));
        assert_eq!(followed.status(), lash::TurnStatus::Answered);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    core.shutdown().await?;
    Ok(())
}
