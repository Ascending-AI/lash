//! Laws of the one ingress (FIG-3600 S5b, D1 §1): what a [`SendHandle`]
//! answers, read from what was recorded, whichever engine drove the input.
//!
//! Every law runs twice: on the in-process engine of the interim SQLite
//! backend, and on lash-restate's engine over the Restate server double.
//!
//! [`SendHandle`]: crate::SendHandle

use super::*;

use futures_util::StreamExt;
use tokio::sync::Notify;

const SEED: u64 = 0x5b_5e_4d;

/// The text the scripted provider holds its answer on until released.
const HELD: &str = "held until released";

/// Counts a held answer dropped before `release` let it return.
struct Abandoned(Option<Arc<AtomicUsize>>);

impl Drop for Abandoned {
    fn drop(&mut self) {
        if let Some(abandoned) = self.0.take() {
            abandoned.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// A provider that answers `echo: <last user text>`, holding the answer to
/// [`HELD`] until `release` is notified, counting its calls and the held
/// answers dropped unreleased.
fn scripted_provider(
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    abandoned: Arc<AtomicUsize>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("send-handle-laws")
        .complete(move |request| {
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            let abandoned = Arc::clone(&abandoned);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let text = last_user_text(&request);
                if text.contains(HELD) {
                    let mut held = Abandoned(Some(abandoned));
                    release.notified().await;
                    held.0 = None;
                }
                Ok(text_response(&format!("echo: {text}")))
            }
        })
        .build()
        .into_handle()
}

/// Which engine drives a law's sends.
#[derive(Clone, Copy, Debug)]
enum Engine {
    /// The interim SQLite backend's in-process engine.
    Sqlite,
    /// lash-restate's engine on the Restate server double.
    Restate,
}

/// A core over the law's engine, and whatever must outlive it: the double is
/// a local that lives to the end of the law (FIG-3723).
struct Fixture {
    core: LashCore,
    _double: Option<lash_restate_test::RestateTestBackend>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    abandoned: Arc<AtomicUsize>,
}

async fn fixture(engine: Engine, batch: usize) -> Result<Fixture> {
    let (backend, double): (lash_core::Backend, _) = match engine {
        Engine::Sqlite => (memory_backend().await.into(), None),
        Engine::Restate => {
            let double = restate_double(SEED).await;
            (double.lash_backend(), Some(double))
        }
    };
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let abandoned = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(batch))
        .provider(scripted_provider(
            Arc::clone(&release),
            Arc::clone(&calls),
            Arc::clone(&abandoned),
        ))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    Ok(Fixture {
        core,
        _double: double,
        release,
        calls,
        abandoned,
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
async fn a_root_whose_live_report_is_gone_answers_its_durable_report(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-durable-report").open().await?;

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

/// A send under a host id whose root already settled commits nothing and
/// answers from that root's evidence (D2 Q6).
async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence(
    engine: Engine,
) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-settled-id").open().await?;

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
    assert_eq!(outcome.status, crate::TurnStatus::Answered);
    let output = outcome.output.expect("a settled root has a report");
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
async fn a_withdrawn_send_answers_cancelled_without_output(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-withdrawn").open().await?;

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
    assert_eq!(outcome.status, crate::TurnStatus::Cancelled);
    assert!(
        outcome.output.is_none(),
        "no turn applied a withdrawn input"
    );
    assert_eq!(outcome.root, None, "no root took a withdrawn input");
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
async fn an_input_answered_inside_another_root_resolves_answered_with_that_root(
    engine: Engine,
) -> Result<()> {
    let fixture = fixture(engine, 4).await?;
    let session = fixture.core.session("send-shared-root").open().await?;

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
    assert_eq!(third.status, crate::TurnStatus::Answered);
    assert_eq!(third.root, Some(lash_core::TurnId::from("second-root")));
    let remote = third.to_remote(&session.session_id(), &third_input);
    remote.validate().expect("the remote outcome is consistent");
    assert_eq!(
        remote.root_id,
        Some(lash_core::TurnId::from("second-root")),
        "a transport re-attaches through the root that answered"
    );
    let third = third.output.expect("an answered input has a report");
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

/// A host that lets go of the core, its sessions and its handles has stopped
/// that worker: a turn the core's in-process engine was still driving stops
/// with it, so the session's lane is left to a peer's takeover rather than
/// held by a drive nothing owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_stops_once_its_host_lets_go_of_the_core() -> Result<()> {
    let Fixture {
        core,
        release: _release,
        calls,
        abandoned,
        ..
    } = fixture(Engine::Sqlite, 1).await?;
    let session = core.session("send-host-gone").open().await?;
    let handle = session.send(TurnInput::text(HELD)).id("held-root").await?;
    reaches(&calls, 1, "the provider is called").await;
    assert_eq!(abandoned.load(Ordering::SeqCst), 0);

    drop(handle);
    drop(session);
    drop(core);
    reaches(&abandoned, 1, "the held drive stops with its core").await;
    Ok(())
}

/// A session a host opened and dropped without closing leaves nothing of
/// itself with the core's open-session registry: the registry holds the
/// session's runtime weakly, all of it, so once the host lets go of the core
/// too, the registry, and the core's driver that holds it, are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_session_leaves_nothing_with_its_core() -> Result<()> {
    let fixture = fixture(Engine::Sqlite, 1).await?;
    let residents = Arc::downgrade(&fixture.core.residents);
    let session = fixture.core.session("dropped-unclosed").open().await?;
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
    let fixture = fixture(Engine::Sqlite, 1).await?;
    let session_id = lash_core::SessionId::from("send-observed");
    let host = fixture.core.session(session_id.clone()).open().await?;
    host.send(TurnInput::text("first"))
        .id("observed-first")
        .output()
        .await?;

    let mut policy = lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.session_id = Some(session_id.clone());
    let store = fixture
        .core
        .store_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy,
        })
        .await?;
    let state = crate::persistence::load_persisted_session_state(store.as_ref())
        .await?
        .expect("the first turn committed a head");
    let observer = fixture
        .core
        .session(session_id.clone())
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
    let core = explicit_ephemeral_facets_with_backend_work(super::rlm_core_builder_over(
        memory_backend().await.into(),
    ))
    .provider(
        crate::testing::TestProvider::builder()
            .kind("engine-first-open")
            .complete(|_| async {
                Ok(text_response(
                    "<typescript>\nfinish(\"engine answered\");\n</typescript>",
                ))
            })
            .build()
            .into_handle(),
    )
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let durable = core.session("engine-first").create().await?;
    durable
        .send(TurnInput::text("the engine opens this session first"))
        .id("engine-first-root")
        .output()
        .await?;

    let session = core.session("engine-first").open().await?;
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
    let core = explicit_ephemeral_facets_with_backend_work(super::rlm_core_builder_over(
        memory_backend().await.into(),
    ))
    .provider({
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
    })
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("cancel-past-switch").open().await?;
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
    assert_eq!(outcome.status, crate::TurnStatus::Cancelled);
    Ok(())
}

async fn cancel_finds_the_consuming_root_before_application(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 4).await?;
    let session = fixture.core.session("cancel-bound-input").open().await?;
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
        parts
            .store
            .root_binding(&session.session_id(), &input_id)
            .await?,
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
    assert_eq!(outcome.status, crate::TurnStatus::Cancelled);
    assert_eq!(third.outcome().await?.status, crate::TurnStatus::Cancelled);
    first.outcome().await?;
    Ok(())
}

async fn replay_gaps_reach_both_streams_and_sinks(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-replay-gap").open().await?;
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
    assert_eq!(sunk.status, crate::TurnStatus::Answered);
    assert!(
        !sunk.gaps.is_empty(),
        "a sink cannot take a gap in-stream, so its answer reports it: {:?}",
        sunk.gaps
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
async fn a_host_reattaches_by_its_id_alone(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 4).await?;
    let session = fixture.core.session("send-attach-id").open().await?;

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
    assert_eq!(outcome.status, crate::TurnStatus::Answered);
    let output = outcome.output.expect("an answered input has a report");
    assert!(
        output
            .assistant_message()
            .is_some_and(|reply| reply.contains("third")),
        "{output:?}"
    );
    assert_eq!(
        session.attach_id("third-root").outcome().await?.status,
        crate::TurnStatus::Answered
    );
    // An id nothing was accepted under answers like a withdrawn input.
    let never = durable.attach_id("never-sent").outcome().await?;
    assert_eq!(never.status, crate::TurnStatus::Cancelled);
    assert!(never.output.is_none());
    Ok(())
}

/// A root this follower never observed live answers with a reported
/// Unavailable gap, so its (empty) activity list is not taken for the root's
/// history; a follower that watched it run reports none.
async fn an_unobserved_root_answers_with_a_reported_gap(engine: Engine) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-unobserved-root").open().await?;

    let handle = session
        .send(TurnInput::text("watch me"))
        .id("watched-root")
        .await?;
    let input_id = handle.input_id().clone();
    let watched = handle.outcome().await?;
    assert_eq!(watched.status, crate::TurnStatus::Answered);
    assert!(watched.gaps.is_empty(), "{:?}", watched.gaps);
    assert!(
        !watched
            .output
            .as_ref()
            .expect("an answered root has a report")
            .activities
            .is_empty()
    );

    let unobserved = session.attach(input_id).outcome().await?;
    assert_eq!(unobserved.status, crate::TurnStatus::Answered);
    let output = unobserved.output.expect("an answered root has a report");
    assert!(output.activities.is_empty());
    assert!(
        matches!(
            unobserved.gaps.as_slice(),
            [gap] if gap.reason == lash_core::LiveReplayGapReason::Unavailable
        ),
        "{:?}",
        unobserved.gaps
    );
    Ok(())
}

macro_rules! send_handle_laws {
    ($engine:ident, $engine_variant:expr) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_root_whose_live_report_is_gone_answers_its_durable_report() -> Result<()> {
                super::a_root_whose_live_report_is_gone_answers_its_durable_report($engine_variant)
                    .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence()
            -> Result<()> {
                super::a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence(
                    $engine_variant,
                )
                .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cancel_finds_the_consuming_root_before_application() -> Result<()> {
                super::cancel_finds_the_consuming_root_before_application($engine_variant).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn replay_gaps_reach_both_streams_and_sinks() -> Result<()> {
                super::replay_gaps_reach_both_streams_and_sinks($engine_variant).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_host_reattaches_by_its_id_alone() -> Result<()> {
                super::a_host_reattaches_by_its_id_alone($engine_variant).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_unobserved_root_answers_with_a_reported_gap() -> Result<()> {
                super::an_unobserved_root_answers_with_a_reported_gap($engine_variant).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_withdrawn_send_answers_cancelled_without_output() -> Result<()> {
                super::a_withdrawn_send_answers_cancelled_without_output($engine_variant).await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn an_input_answered_inside_another_root_resolves_answered_with_that_root()
            -> Result<()> {
                super::an_input_answered_inside_another_root_resolves_answered_with_that_root(
                    $engine_variant,
                )
                .await
            }
        }
    };
}

send_handle_laws!(sqlite, Engine::Sqlite);
send_handle_laws!(restate, Engine::Restate);
