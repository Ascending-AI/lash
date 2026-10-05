//! Laws of the one ingress (FIG-3600 S5b, D1 §1): what a [`SendHandle`]
//! answers, read from what was recorded, whichever engine drove the input.
//!
//! Every law runs on lash-restate's engine over the Restate server double.
//!
//! [`SendHandle`]: crate::SendHandle

use super::*;

use futures_util::StreamExt;
use tokio::sync::Notify;

mod pins;

const SEED: u64 = 0x5b_5e_4d;

/// The text the scripted provider holds its answer on until released.
const HELD: &str = "held until released";

/// A provider that answers `echo: <last user text>`, holding the answer to
/// [`HELD`] until `release` is notified, counting its calls.
fn scripted_provider(release: Arc<Notify>, calls: Arc<AtomicUsize>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("send-handle-laws")
        .complete(move |request| {
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let text = last_user_text(&request);
                if text.contains(HELD) {
                    release.notified().await;
                }
                Ok(text_response(&format!("echo: {text}")))
            }
        })
        .build()
        .into_handle()
}

/// A core over the Restate double, and the double, which must outlive it: it
/// is a local that lives to the end of the law (FIG-3723).
struct Fixture {
    core: LashCore,
    _double: lash_restate_test::RestateTestBackend,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

async fn fixture(batch: usize) -> Result<Fixture> {
    fixture_over(batch, |backend| backend).await
}

/// [`fixture`] whose drain takes every eligible input into one run
/// (`DrainMode::All`), for the laws about an input another input's run
/// answers: the default drain gives each input its own run (FIG-4457).
async fn composing_fixture() -> Result<Fixture> {
    fixture_over_with_batching(
        crate::QueuedWorkBatchingConfig::new(4).with_drain_mode(crate::DrainMode::All),
        |backend| backend,
    )
    .await
}

/// [`fixture`] over the double's backend as `layer` rebuilds it.
async fn fixture_over(
    batch: usize,
    layer: impl FnOnce(lash_core::Backend) -> lash_core::Backend,
) -> Result<Fixture> {
    fixture_over_with_batching(crate::QueuedWorkBatchingConfig::new(batch), layer).await
}

async fn fixture_over_with_batching(
    batching: crate::QueuedWorkBatchingConfig,
    layer: impl FnOnce(lash_core::Backend) -> lash_core::Backend,
) -> Result<Fixture> {
    let double = restate_double(SEED).await;
    let backend = layer(double.lash_backend());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(batching)
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    Ok(Fixture {
        core,
        _double: double,
        release,
        calls,
    })
}

/// Wait until the scripted provider has been called `count` times.
async fn provider_called(fixture: &Fixture, count: usize) {
    reaches(&fixture.calls, count, "the provider is called").await;
}

/// Wait until `counter` reaches `count`.
async fn reaches(counter: &AtomicUsize, count: usize, what: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while counter.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect(what);
}

/// A run whose live report is gone from this process answers the report
/// rebuilt from the store: the same outcome, state and acceptance, marked
/// Durable (D1 §1.5 3b, risk R1).
async fn a_run_whose_live_report_is_gone_answers_its_durable_report() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-durable-report").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("durable-report-run").expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    provider_called(&fixture, 1).await;
    // Subscribe while the provider is held, then let the first follower take
    // the live report. The second follower retains the sealed call activity
    // while rebuilding its terminal report from durable state.
    let follower = session.attach(input_id.clone());
    fixture.release.notify_one();
    let live = handle.output().await?;
    assert_eq!(live.result.source, crate::ReportSource::Live);
    assert_eq!(live.status(), crate::TurnStatus::Answered);

    let durable = follower.output().await?;
    assert_eq!(durable.result.source, crate::ReportSource::Durable);
    assert_eq!(durable.status(), crate::TurnStatus::Answered);
    assert_eq!(durable.result.outcome, live.result.outcome);
    assert_eq!(
        durable.assistant_message(),
        Some("echo: held until released")
    );
    assert!(
        durable
            .activities
            .iter()
            .any(|activity| matches!(activity.event, crate::TurnEvent::ModelCallRecorded { .. }))
    );
    durable
        .result
        .to_remote(
            &session.session_id(),
            &"durable-report-run".parse()?,
            &durable.activities,
        )
        .validate()
        .expect("observed sealed calls remain transportable in a durable report");
    assert_eq!(
        durable.result.state.turn_index,
        live.result.state.turn_index
    );
    assert_eq!(
        durable
            .result
            .acceptance
            .as_ref()
            .map(|acceptance| &acceptance.input_id),
        Some(&input_id)
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// An engine whose shifts never answer that they stopped, as a shift that
/// outlives the run a handle follows does: every other call goes to the
/// engine underneath.
struct UnstoppedShifts(Arc<dyn SessionWorkEngine>);

#[async_trait]
impl SessionWorkEngine for UnstoppedShifts {
    fn schedule_shift(&self, session: &SessionId, request: lash_core::engine::ShiftRequestId) {
        self.0.schedule_shift(session, request);
    }

    async fn request_shift(
        &self,
        session: &SessionId,
        request: lash_core::engine::ShiftRequestId,
    ) -> std::result::Result<(), lash_core::engine::EngineRefusal> {
        self.0.request_shift(session, request).await
    }

    fn install_session_shifts(
        &self,
        shifts: Arc<dyn lash_core::SessionShifts>,
    ) -> Arc<dyn lash_core::SessionShifts> {
        self.0.install_session_shifts(shifts)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.0.control()
    }

    async fn await_shift(
        &self,
        _session: &SessionId,
        _request: &lash_core::engine::ShiftRequestId,
    ) -> std::result::Result<lash_core::engine::ShiftOutcome, lash_core::engine::ShiftAbort> {
        std::future::pending().await
    }
}

/// A settled run whose report no execution in this process can still deposit
/// answers from the store at once, although its shift never says it
/// stopped: the follower waits for a live report only while a run here may
/// still deposit one, never on a run that ran elsewhere or whose report is
/// gone (FIG-3843). Before, it waited out the 5 s live-report grace.
async fn a_settled_run_no_execution_here_can_report_answers_at_once() -> Result<()> {
    let fixture = fixture_over(1, |backend| {
        let work = backend.session_work();
        crate::testing::LayeredBackend::over(backend)
            .with_session_work(Arc::new(UnstoppedShifts(work)))
            .into_backend()
    })
    .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-no-grace").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("report me once"))
        .id(crate::TurnId::parse("no-grace-run").expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    let live = handle.output().await?;
    assert_eq!(live.result.source, crate::ReportSource::Live);

    // The live report is taken and no run of this session is under way here,
    // so nothing can deposit another: a handle attached now answers from the
    // store without waiting on the shift that never says it stopped.
    let started = std::time::Instant::now();
    let durable = session.attach(input_id).output().await?;
    let waited = started.elapsed();
    assert_eq!(durable.result.source, crate::ReportSource::Durable);
    assert_eq!(durable.assistant_message(), Some("echo: report me once"));
    assert!(
        waited < std::time::Duration::from_secs(2),
        "a run no execution here can report answers at once, not after the live-report grace: waited {waited:?}"
    );
    Ok(())
}

/// Wraps the deployment store a fixture's catalog hands out.
type StoreMap = Arc<
    dyn Fn(Arc<dyn lash_core::DeploymentStore>) -> Arc<dyn lash_core::DeploymentStore>
        + Send
        + Sync,
>;

/// [`fixture_over`] with the deployment store `map` over the double's.
async fn fixture_with_stores(batch: usize, map: StoreMap) -> Result<Fixture> {
    fixture_over(batch, |backend| {
        crate::testing::LayeredBackend::over(backend)
            .map_session_store_factory(|inner| map(inner))
            .into_backend()
    })
    .await
}

/// The reads one input's followers make while its run binding is awaited:
/// keyed reads of the binding, and full store polls (each reads the input's
/// open row).
#[derive(Default)]
struct BindingReads {
    input: std::sync::Mutex<Option<lash_core::InputId>>,
    keyed: AtomicUsize,
    polls: AtomicUsize,
}

impl BindingReads {
    fn watch(&self, input: &lash_core::InputId) {
        *self.input.lock_recover() = Some(input.clone());
    }

    fn watches(&self, input: &lash_core::InputId) -> bool {
        self.input.lock_recover().as_ref() == Some(input)
    }
}

/// A deployment store counting [`BindingReads`].
struct CountedBindingReads {
    inner: Arc<dyn lash_core::DeploymentStore>,
    reads: Arc<BindingReads>,
}

impl lash_core::DeploymentStoreDecorator for CountedBindingReads {}

#[async_trait]
impl lash_core::RuntimeStoreDecorator for CountedBindingReads {
    type Inner = dyn lash_core::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn run_of_input(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        if self.reads.watches(input) {
            self.reads.keyed.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.run_of_input(session_id, input).await
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::PendingTurnInputRead>, lash_core::StoreError> {
        if self.reads.watches(input) {
            self.reads.polls.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.pending_turn_input(session_id, input).await
    }
}

/// A follower with no resident runtime learns its input's run by one keyed
/// read of the binding at the poll floor, while its full store poll backs
/// off as before: a run that runs on another worker announces its binding
/// to no event this process sees (FIG-3981). Waking on the probe delays
/// neither the probe nor the poll. Before, the follower read the binding
/// only on the backed-off poll.
async fn an_unbound_inputs_durable_follower_probes_its_binding_while_its_poll_backs_off()
-> Result<()> {
    let reads = Arc::new(BindingReads::default());
    let counted = Arc::clone(&reads);
    let fixture = fixture_with_stores(
        1,
        Arc::new(move |inner| {
            Arc::new(CountedBindingReads {
                inner,
                reads: Arc::clone(&counted),
            }) as Arc<_>
        }),
    )
    .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-run-probe").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    // The held run keeps the second input queued, bound to no run.
    let held = session.send(TurnInput::text(HELD)).await?;
    provider_called(&fixture, 1).await;
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
    let keyed = reads.keyed.load(Ordering::SeqCst);
    let polls = reads.polls.load(Ordering::SeqCst);

    fixture.release.notify_one();
    assert_eq!(held.outcome().await?.status(), crate::TurnStatus::Answered);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), following)
        .await
        .expect("the queued input's follower answers once its run executes")
        .expect("the follower task completes")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);

    // Each poll of an unbound input reads the binding twice; every other
    // keyed read is the probe's. The poll backs off 25 ms, 50 ms, .. to a
    // second, so two seconds hold about seven polls and eighty probe ticks.
    let probes = keyed.saturating_sub(2 * polls);
    assert!(
        (4..=20).contains(&polls),
        "the store poll backs off, and a probe that found no run delays it: {polls} polls"
    );
    assert!(
        probes >= 20,
        "the follower probes its binding at the floor: {probes} probes beside {polls} polls"
    );
    Ok(())
}

/// A scope-close ledger that holds every claim until released, as a close
/// the scope owner is slow to take: the run's recorded close step waits on
/// its claim, and so does a reconcile pass. It counts the closes settled.
struct HeldScopeCloses {
    inner: Arc<dyn lash_core::store::ObligationLedger>,
    released: tokio::sync::watch::Receiver<bool>,
    settled: Arc<AtomicUsize>,
}

impl HeldScopeCloses {
    async fn held(&self) {
        let mut released = self.released.clone();
        let _ = released.wait_for(|released| *released).await;
    }
}

#[async_trait]
impl lash_core::store::ObligationLedger for HeldScopeCloses {
    fn kind(&self) -> lash_core::store::ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &lash_core::store::ObligationKey,
        now_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ObligationId>, lash_core::StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ClaimedObligation>, lash_core::StoreError> {
        self.held().await;
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &lash_core::store::ObligationId,
        token: &lash_core::store::ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ClaimedObligation>, lash_core::StoreError>
    {
        self.held().await;
        self.inner.claim(id, token, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &lash_core::store::ObligationId,
        token: &lash_core::store::ClaimToken,
        settlement: lash_core::store::ObligationSettlement,
        now_ms: u64,
    ) -> std::result::Result<lash_core::store::SettleOutcome, lash_core::StoreError> {
        let outcome = self.inner.settle(id, token, settlement, now_ms).await;
        self.settled.fetch_add(1, Ordering::SeqCst);
        outcome
    }

    async fn rearm(
        &self,
        id: &lash_core::store::ObligationId,
        now_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&lash_core::store::ObligationId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::StalledObligation>, lash_core::StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> std::result::Result<u64, lash_core::StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(
        &self,
        id: &lash_core::store::ObligationId,
    ) -> std::result::Result<Option<lash_core::store::ObligationStanding>, lash_core::StoreError>
    {
        self.inner.standing(id).await
    }
}

/// A run's report is handed to its handle at the run's final commit,
/// before the run's scope closes (FIG-3979): the handle answers the live
/// report while the close is still held, and the close runs after. Before,
/// the report was deposited only once the close returned, so the handle
/// waited out the live-report grace and answered the durable report.
async fn a_send_answers_before_its_runs_scope_closes() -> Result<()> {
    let (release, released) = tokio::sync::watch::channel(false);
    let settled = Arc::new(AtomicUsize::new(0));
    let fixture = fixture_over(1, {
        let settled = Arc::clone(&settled);
        move |backend| {
            crate::testing::LayeredBackend::over(backend)
                .map_obligation_ledgers(move |kind, inner| {
                    if kind == lash_core::store::ObligationKind::ScopeClose {
                        Arc::new(HeldScopeCloses {
                            inner,
                            released: released.clone(),
                            settled: Arc::clone(&settled),
                        }) as Arc<dyn lash_core::store::ObligationLedger>
                    } else {
                        inner
                    }
                })
                .into_backend()
        }
    })
    .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-before-close").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("answer me"))
        .id(crate::TurnId::parse("before-close-run").expect("nonblank host identity"))
        .await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), handle.output())
        .await
        .expect("the handle answers while its run's scope close is held")?;
    assert_eq!(output.result.source, crate::ReportSource::Live);
    assert_eq!(output.assistant_message(), Some("echo: answer me"));
    assert_eq!(
        settled.load(Ordering::SeqCst),
        0,
        "the run's scope had not closed when its handle answered"
    );

    release.send_replace(true);
    reaches(&settled, 1, "the run's scope closes once released").await;
    Ok(())
}

/// A send under a host id whose run already settled commits nothing and
/// answers from that run's evidence (D2 Q6).
async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-settled-id").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let first = session
        .send(TurnInput::text("only once"))
        .id(crate::TurnId::parse("settled-run").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(first.assistant_message(), Some("echo: only once"));
    let applied = session.durable().turn_input_applications().await?;
    assert_eq!(applied.len(), 1);
    session
        .durable()
        .send_parts()
        .await?
        .store
        .vacuum()
        .await
        .expect("vacuum retains the settled run's retry evidence");

    let again = session
        .send(TurnInput::text("only once"))
        .id(crate::TurnId::parse("settled-run").expect("nonblank host identity"))
        .await?;
    assert_eq!(again.input_id(), &applied[0].input_id);
    assert_eq!(
        session
            .attach_id(crate::TurnId::parse("settled-run").expect("nonblank host identity"))
            .input_id(),
        again.input_id(),
        "a retry answers the original acceptance, which its id alone addresses"
    );
    let outcome = again.outcome().await?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);
    let output = outcome.output().expect("a settled run has a report");
    assert_eq!(output.assistant_message(), Some("echo: only once"));

    let conflicting = session
        .send(TurnInput::text("different semantic input"))
        .id(crate::TurnId::parse("settled-run").expect("nonblank host identity"))
        .await;
    assert!(
        conflicting.is_err(),
        "a settled id must validate its submission digest"
    );

    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    assert_eq!(session.durable().turn_input_applications().await?, applied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "nothing ran again");
    Ok(())
}

/// An input withdrawn while it waits answers Cancelled with no output, and
/// its narrow form refuses as not settled (D1 §1.6, §1.7).
async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-withdrawn").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let waiting = session
        .send(TurnInput::text("withdraw me"))
        .id(crate::TurnId::parse("withdrawn-run").expect("nonblank host identity"))
        .await?;
    let input_id = waiting.input_id().clone();
    let receipt = waiting.cancel().await?;
    assert!(
        matches!(receipt, crate::CancelReceipt::Withdrawn(_)),
        "a queued input is withdrawn: {receipt:?}"
    );
    fixture.release.notify_one();
    assert_eq!(
        running.output().await?.assistant_message(),
        Some(format!("echo: {HELD}").as_str())
    );

    let outcome = waiting.outcome().await?;
    assert!(
        matches!(outcome, crate::SendOutcome::Withdrawn { .. }),
        "a withdrawn input answers Withdrawn: {outcome:?}"
    );
    assert!(
        matches!(
            session
                .attach_id(crate::TurnId::parse("withdrawn-run").expect("nonblank host identity"))
                .outcome()
                .await?,
            crate::SendOutcome::Withdrawn { .. }
        ),
        "the withdrawal stays on record under its id"
    );
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert!(
        outcome.output().is_none(),
        "no turn applied a withdrawn input"
    );
    assert_eq!(
        outcome.run().cloned(),
        None,
        "no run took a withdrawn input"
    );
    outcome
        .to_remote(&session.session_id(), &input_id)
        .validate()
        .expect("a withdrawn input's remote outcome is consistent");
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
                crate::SendError::NotSettled { input_id: refused, status: crate::TurnStatus::Cancelled }
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
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-shared-run").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id(crate::TurnId::parse("second-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id(crate::TurnId::parse("third-run").expect("nonblank host identity"))
        .await?;
    let third_input = third.input_id().clone();
    fixture.release.notify_one();
    running.output().await?;

    let second = second.output().await?;
    let third = third.outcome().await?;
    assert_eq!(third.status(), crate::TurnStatus::Answered);
    assert_eq!(
        third.run().cloned(),
        Some(lash_core::TurnId::from("second-run"))
    );
    let remote = third.to_remote(&session.session_id(), &third_input);
    remote.validate().expect("the remote outcome is consistent");
    assert_eq!(
        remote.run().cloned(),
        Some(lash_core::TurnId::from("second-run")),
        "a transport re-attaches through the run that answered"
    );
    let third = third.output().expect("an answered input has a report");
    // One turn applied both inputs, so both handles answer its reply.
    assert!(
        third
            .assistant_message()
            .is_some_and(|reply| reply.contains("second") && reply.contains("third")),
        "{third:?}"
    );
    assert_eq!(third.result.outcome, second.result.outcome);

    let applied_by = session
        .durable()
        .turn_input_applications()
        .await?
        .into_iter()
        .find(|application| application.input_id == third_input)
        .map(|application| application.turn_id)
        .expect("the third input was applied");
    assert_eq!(applied_by, lash_core::TurnId::from("second-run"));
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        2,
        "one turn answered both"
    );
    Ok(())
}

/// A session a host opened and dropped without closing leaves nothing of
/// itself with the core's open-session registry: the registry holds the
/// session's runtime weakly, all of it, so once the host lets go of the core
/// too, the registry, and the core's `SessionShifts` that holds it, are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_session_leaves_nothing_with_its_core() -> Result<()> {
    let fixture = fixture(1).await?;
    let residents = Arc::downgrade(&fixture.core.residents);
    let session = fixture
        .core
        .session(crate::SessionId::parse("dropped-unclosed").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("one turn"))
        .id(crate::TurnId::parse("dropped-unclosed-run").expect("nonblank host identity"))
        .output()
        .await?;
    drop(session);
    drop(fixture);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while residents.upgrade().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the dropped session and core release the open-session registry");
    Ok(())
}

/// A shift never runs on a runtime a host opened to read
/// ([`observe_with_state`](crate::SessionBuilder::observe_with_state)): that
/// open admitted nothing under the session's lease, and the session's shifts
/// stay on the host's admitted open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_never_runs_on_a_session_opened_to_observe() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-observed");
    let host = fixture
        .core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    host.send(TurnInput::text("first"))
        .id(crate::TurnId::parse("observed-first").expect("nonblank host identity"))
        .output()
        .await?;

    let policy =
        lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
    let store = lash_core::runtime::admit_session_view(
        &fixture.core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: (&policy).into(),
            head: lash_core::SessionCreationHead::Config,
        },
    )
    .await?;
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the first turn committed a head")
    .state;
    let observer = fixture
        .core
        .session(session_id.clone())
        .created()
        .await
        .observe_with_state(state)
        .await?;
    assert_eq!(observer.read_view().turn_index(), 1);

    host.send(TurnInput::text("second"))
        .id(crate::TurnId::parse("observed-second").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(
        host.read_view().turn_index(),
        2,
        "the shift ran on the host's open"
    );
    assert_eq!(
        observer.read_view().turn_index(),
        1,
        "no shift ran on the observer's runtime"
    );
    Ok(())
}

/// A run admitted while the only open of its session cannot execute still
/// executes (FIG-5091): that open never hosts the run, and the engine opens
/// its own runtime instead of refusing the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_admitted_beside_a_non_executing_open_executes() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-beside-non-executing");
    let builder = fixture.core.session(session_id.clone()).created().await;
    let store =
        crate::session::resolve_existing_session(&fixture.core.store_factory, &session_id).await?;
    let state = crate::session::load_state_from_store(&session_id, &store).await?;
    let bystander = builder.observe_with_state(state).await?;

    let output = fixture
        .core
        .session(session_id)
        .durable()
        .await?
        .send(TurnInput::text("run beside the bystander"))
        .id(crate::TurnId::parse("beside-non-executing").expect("nonblank host identity"))
        .output()
        .await?;
    assert!(
        output
            .assistant_message()
            .is_some_and(|reply| reply.contains("run beside the bystander")),
        "{output:?}"
    );
    assert_eq!(
        bystander.read_view().turn_index(),
        0,
        "the run never executed on the non-executing open"
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
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(
            crate::testing::TestProvider::builder()
                .kind("engine-first-open")
                .complete(|_| async {
                    Ok(text_response(
                        "<typescript>\nfinish(\"engine answered\");\n</typescript>",
                    ))
                })
                .build()
                .into_handle(),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let durable = core
        .session(crate::SessionId::parse("engine-first").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id(crate::TurnId::parse("engine-first-run").expect("nonblank host identity"))
        .output()
        .await?;
    drop(durable);

    // The engine lane releases the session's writer claim when the sent
    // run settles; the host's open races that release under Restate.
    let session = retry_when_claim_frees(|| {
        core.session(crate::SessionId::parse("engine-first").expect("nonblank host identity"))
            .open()
    })
    .await?;
    let again = session
        .send(TurnInput::text("and a host opens it after"))
        .id(crate::TurnId::parse("host-after-run").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(again.status(), crate::TurnStatus::Answered);
    Ok(())
}

/// A cancel through a send's handle reaches its run past a frame switch:
/// the input was applied, and its turn committed, by the run's first
/// physical turn, but the run executes on in its follow-on turn, so the cancel
/// is placed on that turn and the run answers Cancelled.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_reaches_a_run_past_its_frame_switch() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(
            {
                let calls = Arc::clone(&calls);
                crate::testing::TestProvider::builder()
                .kind("cancel-past-frame-switch")
                .complete(move |_| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            return Ok(text_response(&typescript_block(
                                r#"await control.continue_as({ task: "wait to be cancelled" });"#,
                            )));
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
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("cancel-past-switch").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text("switch frames, then wait"))
        .id(crate::TurnId::parse("cancel-past-switch-run").expect("nonblank host identity"))
        .await?;
    reaches(&calls, 2, "the follow-on turn calls the provider").await;

    let receipt = handle.cancel().origin("send-handle-law").await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { run, .. } if run.as_str() == "cancel-past-switch-run"),
        "the cancel reaches the running run: {receipt:?}"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), handle.outcome())
        .await
        .expect("the cancelled run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    Ok(())
}

async fn cancel_finds_the_consuming_run_before_application() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("cancel-bound-input").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let first = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("first-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("consuming-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("batched input"))
        .id(crate::TurnId::parse("batched-id").expect("nonblank host identity"))
        .await?;
    fixture.release.notify_one();
    provider_called(&fixture, 2).await;
    let input_id = third.input_id().clone();
    let parts = session.durable().send_parts().await?;
    assert_eq!(
        parts.store.run_binding(&input_id).await?,
        Some(lash_core::TurnId::from("consuming-run")),
        "the controlled barrier must hold after binding"
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .all(|application| application.input_id != input_id),
        "the controlled barrier must hold before application"
    );
    let receipt = session.attach(input_id).cancel().await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { run, .. }
        if run.as_str() == "consuming-run"),
        "{receipt:?}"
    );
    fixture.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), second.outcome())
        .await
        .expect("the consuming run settles")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        third.outcome().await?.status(),
        crate::TurnStatus::Cancelled
    );
    first.outcome().await?;
    Ok(())
}

async fn replay_gaps_reach_both_streams_and_sinks() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-replay-gap").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("gap-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    drop(
        fixture
            .core
            .live_replay_store
            .prepare_publication(
                &session.session_id(),
                lash_core::SessionRevision::new(0),
                vec![lash_core::LiveReplayEventDraft::new(
                    None::<lash_core::TurnId>,
                    lash_core::SessionObservationEventPayload::AgentFrameSwitched {
                        frame_id: "lost".into(),
                    },
                )],
            )
            .expect("abandon a publication to create a known replay gap"),
    );
    let mut events = handle.events();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
        .await
        .expect("the stream reports its gap")
        .expect("gap item");
    assert!(
        matches!(event, Err(EmbedError::Send(error)) if matches!(*error, crate::SendError::ObservationGap(_)))
    );
    // The sink follows from the same cursor, so it meets the same gap.
    let sink = RecordingEvents::default();
    let followed = tokio::spawn(async move {
        handle
            .outcome_into(&sink)
            .await
            .map(|outcome| (outcome, sink))
    });
    fixture.release.notify_one();
    // The stream goes on past its gap and ends once the run settles.
    let mut after_gap = 0;
    while let Some(item) = tokio::time::timeout(std::time::Duration::from_secs(20), events.next())
        .await
        .expect("the stream ends once the run settles")
    {
        item.expect("one gap, then the run's activity");
        after_gap += 1;
    }
    assert!(after_gap > 0, "the stream observes on past its gap");
    let (sunk, sink) = followed.await.expect("the sink follower")?;
    assert_eq!(sunk.status(), crate::TurnStatus::Answered);
    assert!(
        !sunk.gaps().is_empty(),
        "a sink cannot take a gap in-stream, so its answer reports it: {:?}",
        sunk.gaps()
    );
    assert!(
        !sink.snapshot().await.is_empty(),
        "the sink observes on past its gap"
    );
    Ok(())
}

/// A host re-attaches to its input with nothing but the id it sent under,
/// on a durable session with no resident state, and follows the input into
/// the run that answered it: a keyed input's id is derived from its session
/// and key.
async fn a_host_reattaches_by_its_id_alone() -> Result<()> {
    let fixture = fixture(4).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-attach-id").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let running = session
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("held-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id(crate::TurnId::parse("second-run").expect("nonblank host identity"))
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id(crate::TurnId::parse("third-run").expect("nonblank host identity"))
        .await?;
    let durable = fixture
        .core
        .session(crate::SessionId::parse("send-attach-id").expect("nonblank host identity"))
        .durable()
        .await?;
    let attached =
        durable.attach_id(crate::TurnId::parse("third-run").expect("nonblank host identity"));
    assert_eq!(attached.input_id(), third.input_id());
    assert_eq!(attached.id(), Some(&lash_core::TurnId::from("third-run")));
    fixture.release.notify_one();
    running.outcome().await?;
    second.outcome().await?;

    let outcome = attached.outcome().await?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);
    let output = outcome.output().expect("an answered input has a report");
    assert!(
        output
            .assistant_message()
            .is_some_and(|reply| reply.contains("third")),
        "{output:?}"
    );
    assert_eq!(
        session
            .attach_id(crate::TurnId::parse("third-run").expect("nonblank host identity"))
            .outcome()
            .await?
            .status(),
        crate::TurnStatus::Answered
    );
    Ok(())
}

/// An id lash never accepted answers `NotAccepted`, never `Withdrawn`: lash
/// holds no record under it, so a send under the same id is accepted as new.
/// A withdrawn input keeps its withdrawal on record and answers `Withdrawn`
/// (FIG-5092).
async fn an_id_never_accepted_answers_not_accepted() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-not-accepted").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let never = crate::TurnId::parse("never-sent").expect("nonblank host identity");
    let live = session.attach_id(never.clone()).outcome().await?;
    let durable = session.durable().attach_id(never.clone()).outcome().await?;
    for outcome in [&live, &durable] {
        assert!(
            matches!(outcome, crate::SendOutcome::NotAccepted { .. }),
            "an id nothing was accepted under answers NotAccepted: {outcome:?}"
        );
        assert_eq!(outcome.status(), crate::TurnStatus::NotAccepted);
        assert_eq!(outcome.run(), None);
        assert!(outcome.output().is_none());
    }
    let input_id = session.attach_id(never.clone()).input_id().clone();
    let remote = live.to_remote(&session.session_id(), &input_id);
    remote
        .validate()
        .expect("a never-accepted input's remote outcome is consistent");
    assert!(
        matches!(
            remote,
            lash_remote_protocol::RemoteSendOutcome::NotAccepted { .. }
        ),
        "{remote:?}"
    );
    assert_eq!(
        remote.status(),
        lash_remote_protocol::RemoteTurnStatus::NotAccepted
    );

    // A send under the id is accepted as new and runs.
    let sent = session
        .send(TurnInput::text("sent at last"))
        .id(never.clone())
        .await?
        .outcome()
        .await?;
    assert_eq!(sent.status(), crate::TurnStatus::Answered, "{sent:?}");
    assert_eq!(
        session.attach_id(never).outcome().await?.status(),
        crate::TurnStatus::Answered
    );
    Ok(())
}

/// A run this follower never observed live answers with a reported
/// Unavailable gap, so its (empty) activity list is not taken for the run's
/// history; a follower that watched it run reports none.
async fn an_unobserved_run_answers_with_a_reported_gap() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-unobserved-run").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("watch me"))
        .id(crate::TurnId::parse("watched-run").expect("nonblank host identity"))
        .await?;
    let input_id = handle.input_id().clone();
    let watched = handle.outcome().await?;
    assert_eq!(watched.status(), crate::TurnStatus::Answered);
    assert!(watched.gaps().is_empty(), "{:?}", watched.gaps());
    assert!(
        !watched
            .output()
            .expect("an answered run has a report")
            .activities
            .is_empty()
    );

    let unobserved = session.attach(input_id).outcome().await?;
    assert_eq!(unobserved.status(), crate::TurnStatus::Answered);
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
fn assert_identity_conflict(refused: std::result::Result<Vec<crate::SendHandle>, EmbedError>) {
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
        .session(crate::SessionId::parse("reserved-batch").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("reserved-batch").expect("nonblank host identity"))
        .open()
        .await?;
    for key in [
        "command:refresh_tool_catalog:foreign",
        "process:foreign:event:1:wake",
    ] {
        let refused = session
            .send_batch([
                (
                    crate::TurnId::parse("host:must-roll-back").expect("nonblank host identity"),
                    TurnInput::text("valid member"),
                ),
                (
                    crate::TurnId::parse(key).expect("nonblank host identity"),
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
        crate::QueuedWorkBatchingConfig::new(1).with_max_turn_input_admission(1),
        |backend| backend,
    )
    .await?;
    fixture
        .core
        .session(crate::SessionId::parse("send-batch").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-batch").expect("nonblank host identity"))
        .open()
        .await?;
    let batch = || {
        [
            (
                crate::TurnId::parse("batch-a").expect("nonblank host identity"),
                TurnInput::text("first"),
            ),
            (
                crate::TurnId::parse("batch-b").expect("nonblank host identity"),
                TurnInput::text("second"),
            ),
            (
                crate::TurnId::parse("batch-c").expect("nonblank host identity"),
                TurnInput::text("third"),
            ),
        ]
    };

    let handles = session.send_batch(batch()).await?;
    assert_eq!(
        handles
            .iter()
            .map(crate::SendHandle::id)
            .collect::<Vec<_>>(),
        ["batch-a", "batch-b", "batch-c"]
            .map(|id| Some(lash_core::TurnId::from(id)))
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
        assert_eq!(outcome.status(), crate::TurnStatus::Answered);
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
        assert_eq!(
            handle.outcome().await?.status(),
            crate::TurnStatus::Answered
        );
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
                    crate::TurnId::parse("batch-d").expect("nonblank host identity"),
                    TurnInput::text("new"),
                ),
                (
                    crate::TurnId::parse("batch-b").expect("nonblank host identity"),
                    TurnInput::text("changed"),
                ),
            ])
            .await,
    );
    assert_identity_conflict(
        session
            .send_batch([
                (
                    crate::TurnId::parse("batch-e").expect("nonblank host identity"),
                    TurnInput::text("once"),
                ),
                (
                    crate::TurnId::parse("batch-e").expect("nonblank host identity"),
                    TurnInput::text("twice"),
                ),
            ])
            .await,
    );
    for never in ["batch-d", "batch-e"] {
        let outcome = session
            .attach_id(crate::TurnId::parse(never).expect("nonblank host identity"))
            .outcome()
            .await?;
        assert_eq!(
            outcome.status(),
            crate::TurnStatus::NotAccepted,
            "`{never}` of a refused batch was never accepted"
        );
    }
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    // These are the current creating, resident, and store-only entry verbs.
    let created = fixture
        .core
        .session(crate::SessionId::parse("send-entry-matrix").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let held = created
        .send(TurnInput::text(HELD))
        .id(crate::TurnId::parse("entry-held").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, calls + 1).await;
    let live = fixture
        .core
        .session(crate::SessionId::parse("send-entry-matrix").expect("nonblank host identity"))
        .open()
        .await?;
    let durable = fixture
        .core
        .session(crate::SessionId::parse("send-entry-matrix").expect("nonblank host identity"))
        .durable()
        .await?;
    let entries = vec![
        live.send(TurnInput::text("resident input"))
            .id(crate::TurnId::parse("entry-live").expect("nonblank host identity"))
            .await?,
        durable
            .send(TurnInput::text("durable input"))
            .id(crate::TurnId::parse("entry-durable").expect("nonblank host identity"))
            .await?,
        live.durable()
            .send(TurnInput::text("resident durable input"))
            .id(crate::TurnId::parse("entry-live-durable").expect("nonblank host identity"))
            .await?,
    ];
    let snapshots = entries
        .iter()
        .map(|handle| handle.receipt().clone())
        .collect::<Vec<_>>();
    let cancelled = durable
        .send_batch([
            (
                crate::TurnId::parse("entry-cancel-a").expect("nonblank host identity"),
                TurnInput::text("cancel a"),
            ),
            (
                crate::TurnId::parse("entry-cancel-b").expect("nonblank host identity"),
                TurnInput::text("cancel b"),
            ),
        ])
        .await?;
    for handle in cancelled {
        assert!(matches!(
            handle.cancel().await?,
            crate::CancelReceipt::Withdrawn(_)
        ));
        assert_eq!(
            handle.outcome().await?.status(),
            crate::TurnStatus::Cancelled
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
    assert_eq!(held.outcome().await?.status(), crate::TurnStatus::Answered);
    for (entry, snapshot) in entries.into_iter().zip(&snapshots) {
        let id = entry.id().expect("host id").clone();
        assert_eq!(entry.outcome().await?.status(), crate::TurnStatus::Answered);
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

/// A send whose spec names a model key this host does not serve is refused
/// before the input is accepted, and nothing is enqueued (FIG-4374).
async fn a_send_under_an_unserved_profile_key_is_refused_before_acceptance() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-bad-route").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let error = session
        .send(TurnInput::text("route me nowhere"))
        .model("no-such-model")
        .await
        .map(|_handle| ())
        .expect_err("a key no registration serves is refused at send");
    assert!(
        matches!(&error, EmbedError::Runtime(runtime)
            if runtime.code == lash_core::RuntimeErrorCode::LlmProfileUnknown),
        "the refusal is the typed unknown-model refusal: {error:?}"
    );
    let store = lash_core::runtime::live_session_view(
        &fixture.core.store_factory,
        &lash_core::SessionId::from("send-bad-route"),
    )
    .await?
    .expect("the opened session has a store");
    assert!(
        store.list_pending_turn_inputs().await?.is_empty(),
        "the refused send accepted nothing"
    );
    // The refusal changed nothing: the session's recorded route still serves.
    session
        .send(TurnInput::text("keep the recorded route"))
        .await?;
    Ok(())
}

async fn exact_host_root_settlement(host_id: &str) -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("exact-host-settlement").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let input = TurnInput::text("only once");
    let first = session
        .send(input.clone())
        .id(lash_core::TurnId::fixture(host_id.to_string()))
        .await?;
    let input_id = first.input_id().clone();
    assert_eq!(
        first.outcome().await?.run().cloned(),
        Some(host_id.parse().unwrap())
    );
    let durable = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session
            .run(lash_core::TurnId::fixture(host_id.to_string()).into())
            .outcome(),
    )
    .await
    .expect("the exact host run settles")?;
    assert_eq!(durable.run().cloned(), Some(host_id.parse().unwrap()));
    assert_eq!(durable.status(), crate::TurnStatus::Answered);
    let retry = session
        .send(input)
        .id(lash_core::TurnId::fixture(host_id.to_string()))
        .await?;
    assert_eq!(retry.input_id(), &input_id);
    let retried = tokio::time::timeout(std::time::Duration::from_secs(2), retry.outcome())
        .await
        .expect("the settled host id answers its retry")?;
    assert_eq!(retried.run().cloned(), Some(host_id.parse().unwrap()));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(session.attach_id(lash_core::TurnId::fixture(host_id.to_string())).cancel().await?, crate::CancelReceipt::AlreadySettled { run } if run.as_str() == host_id)
    );
    Ok(())
}

#[cfg(feature = "rlm")]
async fn exact_host_run_frame_switch(host_id: &str, cancel: bool) -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(double.lash_backend()))
        .serve_test_llm_profile(
            {
                let calls = Arc::clone(&calls);
                let release = Arc::clone(&release);
                crate::testing::TestProvider::builder()
                    .kind("exact-host-frame-switch")
                    .complete(move |_| {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        let release = Arc::clone(&release);
                        async move {
                            if call == 0 {
                                return Ok(text_response(&typescript_block(
                                    r#"await control.continue_as({ task: "follow on" });"#,
                                )));
                            }
                            release.notified().await;
                            Ok(text_response(&typescript_block(
                                r#"finish("finished follow on");"#,
                            )))
                        }
                    })
                    .build()
                    .into_handle()
            },
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("exact-host-switch").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let run_handle = session.run(lash_core::TurnId::fixture(host_id.to_string()).into());
    let handle = session
        .send(TurnInput::text("switch frames"))
        .id(lash_core::TurnId::fixture(host_id))
        .await?;
    let input_id = handle.input_id().clone();
    reaches(&calls, 2, "the run executes its second physical turn").await;
    let run = lash_core::TurnId::fixture(host_id);
    let follow_on = lash_core::store::PhysicalTurn::derive_turn_id(&run, 1);
    assert_eq!(
        session
            .durable()
            .send_parts()
            .await?
            .store
            .run_of_input(&input_id)
            .await?,
        Some(run.clone())
    );

    // Publish distinguishable activity on both of this run's turns and on
    // spellings that a suffix parser could confuse with them.
    let markers = [
        run.clone(),
        follow_on.clone(),
        "unrelated".into(),
        if host_id == "job" {
            "job-other:agent-frame:2".into()
        } else {
            "job:agent-frame:2".into()
        },
        lash_core::TurnId::fixture(format!("{host_id}:agent-frame:01")),
        lash_core::TurnId::fixture(format!("{host_id}:agent-frame:+1")),
    ];
    let activities: Vec<_> = markers
        .iter()
        .map(|turn| {
            lash_core::TurnActivity::independent(lash_core::TurnEvent::TurnStarted {
                turn_id: turn.clone(),
            })
        })
        .collect();
    let prepared = core
        .live_replay_store
        .prepare_publication(
            &session.session_id(),
            lash_core::SessionRevision::new(0),
            markers
                .iter()
                .zip(&activities)
                .map(|(turn, activity)| {
                    lash_core::LiveReplayEventDraft::new(
                        Some(lash_core::TurnId::fixture(turn.to_string())),
                        lash_core::SessionObservationEventPayload::TurnActivity(activity.clone()),
                    )
                })
                .collect(),
        )
        .expect("prepare run membership markers");
    core.live_replay_store
        .publish_prepared(prepared)
        .expect("publish run membership markers");

    let mut input_events = handle.events();
    let mut run_events = run_handle.events();
    let input_stream = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = input_events.next().await {
            events.push(event.expect("input activity"));
        }
        events
    });
    let run_stream = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = run_events.next().await {
            events.push(event.expect("run activity"));
        }
        events
    });
    let expected_status = if cancel {
        let receipt = handle.cancel().origin("exact-host-run-law").await?;
        assert!(
            matches!(&receipt, crate::CancelReceipt::Requested { run: requested, .. } if requested == run),
            "{receipt:?}"
        );
        crate::TurnStatus::Cancelled
    } else {
        release.notify_one();
        crate::TurnStatus::Answered
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), handle.outcome())
        .await
        .expect("the exact run answers across the frame switch")?;
    assert_eq!(outcome.run().cloned(), Some(run.clone()));
    assert_eq!(outcome.status(), expected_status);
    for stream in [input_stream, run_stream] {
        let events = tokio::time::timeout(std::time::Duration::from_secs(3), stream)
            .await
            .expect("the run's activity stream finishes")
            .expect("stream task");
        for (index, activity) in activities.iter().enumerate() {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.id == activity.id)
                    .count(),
                usize::from(index < 2),
                "only the exact run's physical turns contribute activity: {host_id}, {}",
                markers[index]
            );
        }
        let started: Vec<_> = events
            .iter()
            .filter_map(|activity| match &activity.event {
                lash_core::TurnEvent::TurnStarted { turn_id } => Some(turn_id),
                _ => None,
            })
            .collect();
        assert!(started.contains(&&run));
        assert!(started.contains(&&follow_on));
    }
    let durable = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        session.run(run.clone().into()).outcome(),
    )
    .await
    .expect("durable resolution follows the exact run's follow-on")?;
    assert_eq!(durable.run().cloned(), Some(run));
    assert_eq!(durable.status(), expected_status);
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
            #[cfg(feature = "rlm")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn frame_switch() -> Result<()> {
                exact_host_run_frame_switch($host, false).await
            }
            #[cfg(feature = "rlm")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cancellation() -> Result<()> {
                exact_host_run_frame_switch($host, true).await
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

macro_rules! send_handle_laws {
    ($engine:ident) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_run_whose_live_report_is_gone_answers_its_durable_report() -> Result<()> {
                super::a_run_whose_live_report_is_gone_answers_its_durable_report().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_settled_run_no_execution_here_can_report_answers_at_once() -> Result<()> {
                super::a_settled_run_no_execution_here_can_report_answers_at_once().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_unbound_inputs_durable_follower_probes_its_binding_while_its_poll_backs_off()
            -> Result<()> {
                super::an_unbound_inputs_durable_follower_probes_its_binding_while_its_poll_backs_off()
                    .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_answers_before_its_runs_scope_closes() -> Result<()> {
                super::a_send_answers_before_its_runs_scope_closes().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence()
            -> Result<()> {
                super::a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cancel_finds_the_consuming_run_before_application() -> Result<()> {
                super::cancel_finds_the_consuming_run_before_application().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn replay_gaps_reach_both_streams_and_sinks() -> Result<()> {
                super::replay_gaps_reach_both_streams_and_sinks().await
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
            async fn an_id_never_accepted_answers_not_accepted() -> Result<()> {
                super::an_id_never_accepted_answers_not_accepted().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_unobserved_run_answers_with_a_reported_gap() -> Result<()> {
                super::an_unobserved_run_answers_with_a_reported_gap().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_under_an_unserved_profile_key_is_refused_before_acceptance() -> Result<()> {
                super::a_send_under_an_unserved_profile_key_is_refused_before_acceptance().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
                super::a_withdrawn_send_answers_cancelled_without_output().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_input_answered_inside_another_run_resolves_answered_with_that_run()
            -> Result<()> {
                super::an_input_answered_inside_another_run_resolves_answered_with_that_run()
                    .await
            }
        }
    };
}

send_handle_laws!(restate);

#[test]
fn a_journaled_send_outcome_requires_the_data_owned_by_its_variant() {
    let invalid = serde_json::json!({
        "status": "Answered", "run": null, "output": null, "gaps": []
    });
    assert!(
        serde_json::from_value::<crate::SendOutcome>(invalid).is_err(),
        "a journaled answered send cannot exist without its run and output"
    );
}
