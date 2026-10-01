//! Laws of the one ingress (FIG-3600 S5b, D1 §1): what a [`SendHandle`]
//! answers, read from what was recorded, whichever engine drove the input.
//!
//! Every law runs on lash-restate's engine over the Restate server double.
//!
//! [`SendHandle`]: crate::SendHandle

use super::*;

use futures_util::StreamExt;
use tokio::sync::Notify;

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

/// [`fixture`] whose drain takes every eligible input into one root
/// (`DrainMode::All`), for the laws about an input another input's root
/// answers: the default drain gives each input its own root (FIG-4457).
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
    let core = LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    )
    .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(batching)
    .serve_test_model(
        scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
        mock_model_spec(),
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

/// A root whose live report is gone from this process answers the report
/// rebuilt from the store: the same outcome, state and acceptance, marked
/// Durable (D1 §1.5 3b, risk R1).
async fn a_root_whose_live_report_is_gone_answers_its_durable_report() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session("send-durable-report")
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("report me"))
        .id("durable-report-root")
        .await?;
    let input_id = handle.input_id().clone();
    let live = handle.output().await?;
    assert_eq!(live.result.source, crate::ReportSource::Live);
    assert_eq!(live.status(), crate::TurnStatus::Answered);

    // The first handle took the live report; a handle attached afterwards
    // finds none and reads the store.
    let durable = session.attach(input_id.clone()).output().await?;
    assert_eq!(durable.result.source, crate::ReportSource::Durable);
    assert_eq!(durable.status(), crate::TurnStatus::Answered);
    assert_eq!(durable.result.outcome, live.result.outcome);
    assert_eq!(durable.assistant_message(), Some("echo: report me"));
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

/// An engine whose drives never answer that they stopped, as a drive that
/// outlives the root a handle follows does: every other call goes to the
/// engine underneath.
struct UnstoppedDrives(Arc<dyn SessionWorkEngine>);

#[async_trait]
impl SessionWorkEngine for UnstoppedDrives {
    fn schedule_drive(&self, session: &SessionId, request: lash_core::engine::DriveRequestId) {
        self.0.schedule_drive(session, request);
    }

    async fn request_drive(
        &self,
        session: &SessionId,
        request: lash_core::engine::DriveRequestId,
    ) -> std::result::Result<(), lash_core::engine::EngineRefusal> {
        self.0.request_drive(session, request).await
    }

    fn install_session_driver(
        &self,
        driver: Arc<dyn lash_core::SessionDriver>,
    ) -> Arc<dyn lash_core::SessionDriver> {
        self.0.install_session_driver(driver)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.0.control()
    }

    async fn await_drive(
        &self,
        _session: &SessionId,
        _request: &lash_core::engine::DriveRequestId,
    ) -> std::result::Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        std::future::pending().await
    }
}

/// A settled root whose report no run in this process can still deposit
/// answers from the store at once, although its drive never says it
/// stopped: the follower waits for a live report only while a run here may
/// still deposit one, never on a root that ran elsewhere or whose report is
/// gone (FIG-3843). Before, it waited out the 5 s live-report grace.
async fn a_settled_root_no_run_here_can_report_answers_at_once() -> Result<()> {
    let fixture = fixture_over(1, |backend| {
        let work = backend.session_work();
        crate::testing::LayeredBackend::over(backend)
            .with_session_work(Arc::new(UnstoppedDrives(work)))
            .into_backend()
    })
    .await?;
    let session = fixture
        .core
        .session("send-no-grace")
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("report me once"))
        .id("no-grace-root")
        .await?;
    let input_id = handle.input_id().clone();
    let live = handle.output().await?;
    assert_eq!(live.result.source, crate::ReportSource::Live);

    // The live report is taken and no run of this session is under way here,
    // so nothing can deposit another: a handle attached now answers from the
    // store without waiting on the drive that never says it stopped.
    let started = std::time::Instant::now();
    let durable = session.attach(input_id).output().await?;
    let waited = started.elapsed();
    assert_eq!(durable.result.source, crate::ReportSource::Durable);
    assert_eq!(durable.assistant_message(), Some("echo: report me once"));
    assert!(
        waited < std::time::Duration::from_secs(2),
        "a root no run here can report answers at once, not after the live-report grace: waited {waited:?}"
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

/// The reads one input's followers make while its root binding is awaited:
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

    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        if self.reads.watches(input) {
            self.reads.keyed.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.root_of_input(session_id, input).await
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

/// A follower with no resident runtime learns its input's root by one keyed
/// read of the binding at the poll floor, while its full store poll backs
/// off as before: a root that runs on another worker announces its binding
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
        .session("send-root-probe")
        .created()
        .await
        .open()
        .await?;

    // The held root keeps the second input queued, bound to no root.
    let held = session.send(TurnInput::text(HELD)).await?;
    provider_called(&fixture, 1).await;
    let queued = session
        .send(TurnInput::text("queued behind the held root"))
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
        .expect("the queued input's follower answers once its root runs")
        .expect("the follower task completes")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);

    // Each poll of an unbound input reads the binding twice; every other
    // keyed read is the probe's. The poll backs off 25 ms, 50 ms, .. to a
    // second, so two seconds hold about seven polls and eighty probe ticks.
    let probes = keyed.saturating_sub(2 * polls);
    assert!(
        (4..=20).contains(&polls),
        "the store poll backs off, and a probe that found no root delays it: {polls} polls"
    );
    assert!(
        probes >= 20,
        "the follower probes its binding at the floor: {probes} probes beside {polls} polls"
    );
    Ok(())
}

/// A scope-close ledger that holds every claim until released, as a close
/// the scope owner is slow to take: the root's recorded close step waits on
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

/// A root's report is handed to its handle at the root's final commit,
/// before the root's scope closes (FIG-3979): the handle answers the live
/// report while the close is still held, and the close runs after. Before,
/// the report was deposited only once the close returned, so the handle
/// waited out the live-report grace and answered the durable report.
async fn a_send_answers_before_its_roots_scope_closes() -> Result<()> {
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
        .session("send-before-close")
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("answer me"))
        .id("before-close-root")
        .await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), handle.output())
        .await
        .expect("the handle answers while its root's scope close is held")?;
    assert_eq!(output.result.source, crate::ReportSource::Live);
    assert_eq!(output.assistant_message(), Some("echo: answer me"));
    assert_eq!(
        settled.load(Ordering::SeqCst),
        0,
        "the root's scope had not closed when its handle answered"
    );

    release.send_replace(true);
    reaches(&settled, 1, "the root's scope closes once released").await;
    Ok(())
}

/// A send under a host id whose root already settled commits nothing and
/// answers from that root's evidence (D2 Q6).
async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session("send-settled-id")
        .created()
        .await
        .open()
        .await?;

    let first = session
        .send(TurnInput::text("only once"))
        .id("settled-root")
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
        .expect("vacuum retains the settled root's retry evidence");

    let again = session
        .send(TurnInput::text("only once"))
        .id("settled-root")
        .await?;
    assert_eq!(again.input_id(), &applied[0].input_id);
    assert_eq!(
        session.attach_id("settled-root").input_id(),
        again.input_id(),
        "a retry answers the original acceptance, which its id alone addresses"
    );
    let outcome = again.outcome().await?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);
    let output = outcome.output().expect("a settled root has a report");
    assert_eq!(output.assistant_message(), Some("echo: only once"));

    let conflicting = session
        .send(TurnInput::text("different semantic input"))
        .id("settled-root")
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
        .session("send-withdrawn")
        .created()
        .await
        .open()
        .await?;

    let running = session.send(TurnInput::text(HELD)).id("held-root").await?;
    provider_called(&fixture, 1).await;
    let waiting = session
        .send(TurnInput::text("withdraw me"))
        .id("withdrawn-root")
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
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert!(
        outcome.output().is_none(),
        "no turn applied a withdrawn input"
    );
    assert_eq!(
        outcome.root().cloned(),
        None,
        "no root took a withdrawn input"
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

/// An input a drive answers inside another input's root resolves Answered
/// with that root's turn: the handle reads which root applied it, never
/// "my drive ran it", so it neither ceded nor refused (review of #2290,
/// MEDIUM-4).
async fn an_input_answered_inside_another_root_resolves_answered_with_that_root() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = fixture
        .core
        .session("send-shared-root")
        .created()
        .await
        .open()
        .await?;

    let running = session.send(TurnInput::text(HELD)).id("held-root").await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id("second-root")
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id("third-root")
        .await?;
    let third_input = third.input_id().clone();
    fixture.release.notify_one();
    running.output().await?;

    let second = second.output().await?;
    let third = third.outcome().await?;
    assert_eq!(third.status(), crate::TurnStatus::Answered);
    assert_eq!(
        third.root().cloned(),
        Some(lash_core::TurnId::from("second-root"))
    );
    let remote = third.to_remote(&session.session_id(), &third_input);
    remote.validate().expect("the remote outcome is consistent");
    assert_eq!(
        remote.root().cloned(),
        Some(lash_core::TurnId::from("second-root")),
        "a transport re-attaches through the root that answered"
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
    assert_eq!(applied_by, lash_core::TurnId::from("second-root"));
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
/// too, the registry, and the core's driver that holds it, are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_session_leaves_nothing_with_its_core() -> Result<()> {
    let fixture = fixture(1).await?;
    let residents = Arc::downgrade(&fixture.core.residents);
    let session = fixture
        .core
        .session("dropped-unclosed")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("one turn"))
        .id("dropped-unclosed-root")
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

/// A drive never runs on a runtime a host opened to read
/// ([`observe_with_state`](crate::SessionBuilder::observe_with_state)): that
/// open admitted nothing under the session's lease, and the session's drives
/// stay on the host's admitted open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_never_runs_on_a_session_opened_to_observe() -> Result<()> {
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
        .id("observed-first")
        .output()
        .await?;

    let mut policy =
        lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024));
    policy.session_id = Some(session_id.clone());
    let store = lash_core::runtime::admit_session_view(
        &fixture.core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: (&policy).into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
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
        .id("observed-second")
        .output()
        .await?;
    assert_eq!(
        host.read_view().turn_index(),
        2,
        "the drive ran on the host's open"
    );
    assert_eq!(
        observer.read_view().turn_index(),
        1,
        "no drive ran on the observer's runtime"
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
        .serve_test_model(
            crate::testing::TestProvider::builder()
                .kind("engine-first-open")
                .complete(|_| async {
                    Ok(text_response(
                        "<typescript>\nfinish(\"engine answered\");\n</typescript>",
                    ))
                })
                .build()
                .into_handle(),
            mock_model_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let durable = core
        .session("engine-first")
        .create(crate::SessionCreation::default())
        .await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id("engine-first-root")
        .output()
        .await?;
    drop(durable);

    // The engine lane releases the session's writer claim when the sent
    // root settles; the host's open races that release under Restate.
    let session = retry_when_claim_frees(|| core.session("engine-first").open()).await?;
    let again = session
        .send(TurnInput::text("and a host opens it after"))
        .id("host-after-root")
        .output()
        .await?;
    assert_eq!(again.status(), crate::TurnStatus::Answered);
    Ok(())
}

/// A cancel through a send's handle reaches its root past a frame switch:
/// the input was applied, and its turn committed, by the root's first
/// physical turn, but the root runs on in its follow-on turn, so the cancel
/// is placed on that turn and the root answers Cancelled.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_reaches_a_root_past_its_frame_switch() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(double.lash_backend()))
        .serve_test_model(
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
            mock_model_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("cancel-past-switch")
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text("switch frames, then wait"))
        .id("cancel-past-switch-root")
        .await?;
    reaches(&calls, 2, "the follow-on turn calls the provider").await;

    let receipt = handle.cancel().origin("send-handle-law").await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { root, .. } if root.as_str() == "cancel-past-switch-root"),
        "the cancel reaches the running root: {receipt:?}"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), handle.outcome())
        .await
        .expect("the cancelled root answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    Ok(())
}

async fn cancel_finds_the_consuming_root_before_application() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = fixture
        .core
        .session("cancel-bound-input")
        .created()
        .await
        .open()
        .await?;
    let first = session.send(TurnInput::text(HELD)).id("first-root").await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text(HELD))
        .id("consuming-root")
        .await?;
    let third = session
        .send(TurnInput::text("batched input"))
        .id("batched-id")
        .await?;
    fixture.release.notify_one();
    provider_called(&fixture, 2).await;
    let input_id = third.input_id().clone();
    let parts = session.durable().send_parts().await?;
    assert_eq!(
        parts.store.root_binding(&input_id).await?,
        Some(lash_core::TurnId::from("consuming-root")),
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
        matches!(&receipt, crate::CancelReceipt::Requested { root, .. }
        if root.as_str() == "consuming-root"),
        "{receipt:?}"
    );
    fixture.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), second.outcome())
        .await
        .expect("the consuming root settles")?;
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
        .session("send-replay-gap")
        .created()
        .await
        .open()
        .await?;
    let handle = session.send(TurnInput::text(HELD)).id("gap-root").await?;
    provider_called(&fixture, 1).await;
    drop(
        fixture
            .core
            .live_replay_store
            .prepare_publication(
                &session.session_id(),
                lash_core::SessionRevision::new(0),
                vec![lash_core::LiveReplayEventDraft::new(
                    None::<String>,
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
    // The stream goes on past its gap and ends once the root settles.
    let mut after_gap = 0;
    while let Some(item) = tokio::time::timeout(std::time::Duration::from_secs(20), events.next())
        .await
        .expect("the stream ends once the root settles")
    {
        item.expect("one gap, then the root's activity");
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
/// the root that answered it: a keyed input's id is derived from its session
/// and key.
async fn a_host_reattaches_by_its_id_alone() -> Result<()> {
    let fixture = fixture(4).await?;
    let session = fixture
        .core
        .session("send-attach-id")
        .created()
        .await
        .open()
        .await?;

    let running = session.send(TurnInput::text(HELD)).id("held-root").await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text("second"))
        .id("second-root")
        .await?;
    let third = session
        .send(TurnInput::text("third"))
        .id("third-root")
        .await?;
    let durable = fixture.core.session("send-attach-id").durable().await?;
    let attached = durable.attach_id("third-root");
    assert_eq!(attached.input_id(), third.input_id());
    assert_eq!(attached.id(), Some(&lash_core::TurnId::from("third-root")));
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
        session.attach_id("third-root").outcome().await?.status(),
        crate::TurnStatus::Answered
    );
    // An id nothing was accepted under answers like a withdrawn input.
    let never = durable.attach_id("never-sent").outcome().await?;
    assert_eq!(never.status(), crate::TurnStatus::Cancelled);
    assert!(never.output().is_none());
    Ok(())
}

/// A root this follower never observed live answers with a reported
/// Unavailable gap, so its (empty) activity list is not taken for the root's
/// history; a follower that watched it run reports none.
async fn an_unobserved_root_answers_with_a_reported_gap() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session("send-unobserved-root")
        .created()
        .await
        .open()
        .await?;

    let handle = session
        .send(TurnInput::text("watch me"))
        .id("watched-root")
        .await?;
    let input_id = handle.input_id().clone();
    let watched = handle.outcome().await?;
    assert_eq!(watched.status(), crate::TurnStatus::Answered);
    assert!(watched.gaps().is_empty(), "{:?}", watched.gaps());
    assert!(
        !watched
            .output()
            .expect("an answered root has a report")
            .activities
            .is_empty()
    );

    let unobserved = session.attach(input_id).outcome().await?;
    assert_eq!(unobserved.status(), crate::TurnStatus::Answered);
    let output = unobserved.output().expect("an answered root has a report");
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
        .session("reserved-batch")
        .create(crate::SessionCreation::default())
        .await?;
    let session = fixture.core.session("reserved-batch").open().await?;
    for key in [
        "command:refresh_tool_catalog:foreign",
        "process:foreign:event:1:wake",
    ] {
        let refused = session
            .send_batch([
                ("host:must-roll-back", TurnInput::text("valid member")),
                (key, TurnInput::text("reserved member")),
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
        "a refused batch drives nothing"
    );
    Ok(())
}

/// One batch answers one handle per input, in request order, and each input
/// is answered by the root that applied it (FIG-3842). Resending the batch
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
        .session("send-batch")
        .create(crate::SessionCreation::default())
        .await?;
    let session = fixture.core.session("send-batch").open().await?;
    let batch = || {
        [
            ("batch-a", TurnInput::text("first")),
            ("batch-b", TurnInput::text("second")),
            ("batch-c", TurnInput::text("third")),
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
            outcome.root().is_some() && outcome.output().is_some(),
            "{outcome:?}"
        );
    }
    let calls = fixture.calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 3,
        "the configured input cap drives one input per root"
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
                ("batch-d", TurnInput::text("new")),
                ("batch-b", TurnInput::text("changed")),
            ])
            .await,
    );
    assert_identity_conflict(
        session
            .send_batch([
                ("batch-e", TurnInput::text("once")),
                ("batch-e", TurnInput::text("twice")),
            ])
            .await,
    );
    for never in ["batch-d", "batch-e"] {
        let outcome = session.attach_id(never).outcome().await?;
        assert_eq!(
            outcome.status(),
            crate::TurnStatus::Cancelled,
            "`{never}` of a refused batch was never accepted"
        );
    }
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    // These are the current creating, resident, and store-only entry verbs.
    let created = fixture
        .core
        .session("send-entry-matrix")
        .create(crate::SessionCreation::default())
        .await?;
    let held = created.send(TurnInput::text(HELD)).id("entry-held").await?;
    provider_called(&fixture, calls + 1).await;
    let live = fixture.core.session("send-entry-matrix").open().await?;
    let durable = fixture.core.session("send-entry-matrix").durable().await?;
    let entries = vec![
        live.send(TurnInput::text("resident input"))
            .id("entry-live")
            .await?,
        durable
            .send(TurnInput::text("durable input"))
            .id("entry-durable")
            .await?,
        live.durable()
            .send(TurnInput::text("resident durable input"))
            .id("entry-live-durable")
            .await?,
    ];
    let snapshots = entries
        .iter()
        .map(|handle| handle.receipt().clone())
        .collect::<Vec<_>>();
    let cancelled = durable
        .send_batch([
            ("entry-cancel-a", TurnInput::text("cancel a")),
            ("entry-cancel-b", TurnInput::text("cancel b")),
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
        "held root and three uncancelled admissions remain"
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
async fn a_send_under_an_unserved_model_key_is_refused_before_acceptance() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session("send-bad-route")
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
            if runtime.code == lash_core::RuntimeErrorCode::ModelUnknown),
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
        .session("exact-host-settlement")
        .created()
        .await
        .open()
        .await?;
    let input = TurnInput::text("only once");
    let first = session.send(input.clone()).id(host_id).await?;
    let input_id = first.input_id().clone();
    assert_eq!(first.outcome().await?.root().cloned(), Some(host_id.into()));
    let durable = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.root(host_id).outcome(),
    )
    .await
    .expect("the exact host root settles")?;
    assert_eq!(durable.root().cloned(), Some(host_id.into()));
    assert_eq!(durable.status(), crate::TurnStatus::Answered);
    let retry = session.send(input).id(host_id).await?;
    assert_eq!(retry.input_id(), &input_id);
    let retried = tokio::time::timeout(std::time::Duration::from_secs(2), retry.outcome())
        .await
        .expect("the settled host id answers its retry")?;
    assert_eq!(retried.root().cloned(), Some(host_id.into()));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(session.attach_id(host_id).cancel().await?, crate::CancelReceipt::AlreadySettled { root } if root.as_str() == host_id)
    );
    Ok(())
}

#[cfg(feature = "rlm")]
async fn exact_host_root_frame_switch(host_id: &str, cancel: bool) -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(double.lash_backend()))
        .serve_test_model(
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
            mock_model_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("exact-host-switch")
        .created()
        .await
        .open()
        .await?;
    let root_handle = session.root(host_id);
    let handle = session
        .send(TurnInput::text("switch frames"))
        .id(host_id)
        .await?;
    let input_id = handle.input_id().clone();
    reaches(&calls, 2, "the root runs its second physical turn").await;
    let root = lash_core::TurnId::from(host_id);
    let follow_on = lash_core::store::PhysicalTurn::derive_turn_id(&root, 1);
    assert_eq!(
        session
            .durable()
            .send_parts()
            .await?
            .store
            .root_of_input(&input_id)
            .await?,
        Some(root.clone())
    );

    // Publish distinguishable activity on both of this root's turns and on
    // spellings that a suffix parser could confuse with them.
    let markers = [
        root.clone(),
        follow_on.clone(),
        "unrelated".into(),
        if host_id == "job" {
            "job-other:agent-frame:2".into()
        } else {
            "job:agent-frame:2".into()
        },
        format!("{host_id}:agent-frame:01").into(),
        format!("{host_id}:agent-frame:+1").into(),
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
                        Some(turn.to_string()),
                        lash_core::SessionObservationEventPayload::TurnActivity(activity.clone()),
                    )
                })
                .collect(),
        )
        .expect("prepare root membership markers");
    core.live_replay_store
        .publish_prepared(prepared)
        .expect("publish root membership markers");

    let mut input_events = handle.events();
    let mut root_events = root_handle.events();
    let input_stream = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = input_events.next().await {
            events.push(event.expect("input activity"));
        }
        events
    });
    let root_stream = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = root_events.next().await {
            events.push(event.expect("root activity"));
        }
        events
    });
    let expected_status = if cancel {
        let receipt = handle.cancel().origin("exact-host-root-law").await?;
        assert!(
            matches!(&receipt, crate::CancelReceipt::Requested { root: requested, .. } if requested == root),
            "{receipt:?}"
        );
        crate::TurnStatus::Cancelled
    } else {
        release.notify_one();
        crate::TurnStatus::Answered
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), handle.outcome())
        .await
        .expect("the exact root answers across the frame switch")?;
    assert_eq!(outcome.root().cloned(), Some(root.clone()));
    assert_eq!(outcome.status(), expected_status);
    for stream in [input_stream, root_stream] {
        let events = tokio::time::timeout(std::time::Duration::from_secs(3), stream)
            .await
            .expect("the root's activity stream finishes")
            .expect("stream task");
        for (index, activity) in activities.iter().enumerate() {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.id == activity.id)
                    .count(),
                usize::from(index < 2),
                "only the exact root's physical turns contribute activity: {host_id}, {}",
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
        assert!(started.contains(&&root));
        assert!(started.contains(&&follow_on));
    }
    let durable = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        session.root(root.clone()).outcome(),
    )
    .await
    .expect("durable resolution follows the exact root's follow-on")?;
    assert_eq!(durable.root().cloned(), Some(root));
    assert_eq!(durable.status(), expected_status);
    Ok(())
}

macro_rules! exact_host_root_laws {
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
                exact_host_root_frame_switch($host, false).await
            }
            #[cfg(feature = "rlm")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cancellation() -> Result<()> {
                exact_host_root_frame_switch($host, true).await
            }
        }
    };
}

mod exact_host_roots {
    use super::*;
    exact_host_root_laws!(plain, "job");
    exact_host_root_laws!(canonical_suffix, "job:agent-frame:1");
    exact_host_root_laws!(leading_zero_suffix, "job:agent-frame:01");
    exact_host_root_laws!(signed_suffix, "job:agent-frame:+1");
}

macro_rules! send_handle_laws {
    ($engine:ident) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_root_whose_live_report_is_gone_answers_its_durable_report() -> Result<()> {
                super::a_root_whose_live_report_is_gone_answers_its_durable_report().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_settled_root_no_run_here_can_report_answers_at_once() -> Result<()> {
                super::a_settled_root_no_run_here_can_report_answers_at_once().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_unbound_inputs_durable_follower_probes_its_binding_while_its_poll_backs_off()
            -> Result<()> {
                super::an_unbound_inputs_durable_follower_probes_its_binding_while_its_poll_backs_off()
                    .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_answers_before_its_roots_scope_closes() -> Result<()> {
                super::a_send_answers_before_its_roots_scope_closes().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence()
            -> Result<()> {
                super::a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cancel_finds_the_consuming_root_before_application() -> Result<()> {
                super::cancel_finds_the_consuming_root_before_application().await
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
            async fn an_unobserved_root_answers_with_a_reported_gap() -> Result<()> {
                super::an_unobserved_root_answers_with_a_reported_gap().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_under_an_unserved_model_key_is_refused_before_acceptance() -> Result<()> {
                super::a_send_under_an_unserved_model_key_is_refused_before_acceptance().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
                super::a_withdrawn_send_answers_cancelled_without_output().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_input_answered_inside_another_root_resolves_answered_with_that_root()
            -> Result<()> {
                super::an_input_answered_inside_another_root_resolves_answered_with_that_root()
                    .await
            }
        }
    };
}

send_handle_laws!(restate);

#[test]
fn a_journaled_send_outcome_requires_the_data_owned_by_its_variant() {
    let invalid = serde_json::json!({
        "status": "Answered", "root": null, "output": null, "gaps": []
    });
    assert!(
        serde_json::from_value::<crate::SendOutcome>(invalid).is_err(),
        "a journaled answered send cannot exist without its root and output"
    );
}
