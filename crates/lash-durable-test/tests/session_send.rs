//! Queued withdrawal and running cancellation through the facade (ADR 0039,
//! ADR 0101 A2), ported from the L9c and L9e laws retired with their host;
//! and the host's drain policy over queued input (ADR 0101 §5.2, FIG-5293).
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use served::{Tier, WATCHDOG, World};

const RUNNING: &str = "keep me running";
const QUEUED: &str = "withdraw me";

/// The provider holds the first run until its cancellation unwinds it.
/// Every entry is counted, including an erroneous admission of the queue.
#[derive(Default)]
struct HeldModel {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
}

impl HeldModel {
    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("cancel-held-model")
            .complete(move |request: lash_core::llm::types::LlmRequest| {
                let model = Arc::clone(&model);
                async move {
                    model.calls.fetch_add(1, Ordering::SeqCst);
                    model.started.notify_one();
                    if format!("{:?}", request.messages).contains(QUEUED) {
                        return Ok(served::text(&request, "the withdrawn input ran"));
                    }
                    std::future::pending().await
                }
            })
            .build()
            .into_handle()
    }
}

async fn world(tier: Tier) -> Option<(World, lash::DurableSession, Arc<HeldModel>)> {
    let model = Arc::new(HeldModel::default());
    let world = World::with_model(tier, Vec::new(), model.provider(), |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await?;
    let session = world.session("withdraw-cancel", served::spec(8)).await;
    Some((world, session, model))
}

async fn assert_stopped(
    session: &lash::DurableSession,
    running: lash::SendHandle,
    queued: lash::SendHandle,
    model: &HeldModel,
) {
    let input = queued.input_id().clone();
    let (running, queued) = tokio::join!(running.outcome(), queued.outcome());
    let running = running.expect("the running send settles");
    let queued = queued.expect("the queued send settles");
    assert_eq!(running.status(), lash::TurnStatus::Cancelled);
    assert!(
        running.output().is_some(),
        "the running run commits its stop"
    );
    assert!(matches!(queued, lash::SendOutcome::Withdrawn { .. }));
    assert!(queued.output().is_none(), "withdrawal applies no turn");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "the queue never calls the model"
    );
    let applied = session
        .turn_input_applications()
        .await
        .expect("read applications");
    assert!(
        applied
            .iter()
            .all(|application| application.input_id != input),
        "the withdrawn input is never applied: {applied:?}"
    );
    let transcript = session
        .transcript()
        .await
        .expect("read the committed transcript");
    assert!(
        !format!("{transcript:?}").contains(QUEUED),
        "the queue never reaches history"
    );
}

/// Draft reconciliation reads the retained terminal, without addressing a run.
async fn withdrawal_tombstone(
    session: &lash::DurableSession,
    input: &lash::InputId,
) -> lash_core::PendingTurnInput {
    let mut receipts = session
        .cancel_pending_turn_inputs([lash_core::PendingTurnInputCancelTarget::input_id(
            input.to_string(),
        )])
        .await
        .expect("read the cancelled tombstone through draft reconciliation");
    let receipt = receipts.pop().expect("the input has one receipt");
    let lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input) = receipt.outcome else {
        panic!("the withdrawal tombstone is retained");
    };
    input
}

/// Withdraw a queued run by its host id, before it has any admitted run row.
/// The facade retains the typed withdrawal and repeated input cancellation
/// reads the same durable tombstone; cancelling the admitted input stops its run.
async fn withdraw_while_queued_vs_cancel_while_running(tier: Tier) {
    let Some((world, session, model)) = world(tier).await else {
        return;
    };
    tokio::time::timeout(WATCHDOG, async {
        let running = session
            .send(lash::TurnInput::text(RUNNING))
            .await
            .expect("send first");
        model.started.notified().await;
        let id = lash::TurnId::try_from("queued-run".to_owned()).expect("a host id");
        let queued = session
            .send(lash::TurnInput::text(QUEUED))
            .id(id.clone())
            .await
            .expect("send second");
        assert_eq!(
            queued.run().await.expect("read binding"),
            None,
            "the queue is unadmitted"
        );
        let receipt = session
            .run(lash::RunId::from(id))
            .cancel()
            .await
            .expect("cancel the queued run");
        assert!(
            matches!(&receipt, lash::CancelReceipt::Withdrawn { run, input: Some(input) }
                if run == queued.id().expect("the host named the queued run") && input == queued.input_id()),
            "a queued run withdraws: {receipt:?}"
        );
        let before = withdrawal_tombstone(&session, queued.input_id()).await;
        let terminal = before.terminal().expect("the withdrawal has a terminal");
        assert_eq!(terminal.cause, lash_core::store::IngressTerminalCause::Cancelled);
        let again = queued.cancel().await.expect("repeat the queued cancellation");
        assert!(matches!(again, lash::CancelReceipt::UnknownOrRevoked),
            "a withdrawn run accepts no new cancel: {again:?}");
        let after = withdrawal_tombstone(&session, queued.input_id()).await;
        assert_eq!(after.terminal(), Some(terminal), "the tombstone is unchanged");
        let receipt = running.cancel().await.expect("cancel the admitted input");
        assert!(
            matches!(&receipt, lash::CancelReceipt::Cancelled { receipt, .. }
            if matches!(receipt.outcome, lash::TurnCancelOutcome::Requested(_))),
            "the admitted input addresses its running run: {receipt:?}"
        );
        assert_stopped(&session, running, queued, &model).await;
    })
    .await
    .expect("deadlock watchdog: cancel/withdraw never settled");
    world.shutdown().await;
}

/// Stop both accepted sends: the first commits a cancelled run and the
/// second retains a withdrawal with no output and no new provider call.
async fn cancelling_both_sends_stops_the_running_run_and_withdraws_the_queued_one(tier: Tier) {
    let Some((world, session, model)) = world(tier).await else {
        return;
    };
    tokio::time::timeout(WATCHDOG, async {
        let running = session
            .send(lash::TurnInput::text(RUNNING))
            .await
            .expect("send first");
        model.started.notified().await;
        let queued = session
            .send(lash::TurnInput::text(QUEUED))
            .await
            .expect("send second");
        let running_id = running.input_id().clone();
        assert!(matches!(
            session
                .attach(running_id.clone())
                .cancel()
                .await
                .expect("stop first"),
            lash::CancelReceipt::Cancelled { .. }
        ));
        assert!(matches!(
            queued.cancel().await.expect("withdraw second"),
            lash::CancelReceipt::Withdrawn { .. }
        ));
        assert_stopped(&session, running, queued, &model).await;
        let again = session
            .attach(running_id)
            .cancel()
            .await
            .expect("stop again");
        assert!(
            matches!(again, lash::CancelReceipt::UnknownOrRevoked),
            "the stopped run is already settled: {again:?}"
        );
    })
    .await
    .expect("deadlock watchdog: both sends never settled");
    world.shutdown().await;
}

const BUSY: &str = "hold the session while the queue fills";
const COALESCED: [&str; 3] = ["coalesced one", "coalesced two", "coalesced three"];

/// The provider holds its first request until the law releases it, answers
/// every request in prose, and keeps every request it was asked, rendered.
#[derive(Default)]
struct GatedModel {
    requests: std::sync::Mutex<Vec<String>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl GatedModel {
    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("coalescing-gated-model")
            .complete(move |request: lash_core::llm::types::LlmRequest| {
                let model = Arc::clone(&model);
                async move {
                    let rendered = format!("{:?}", request.messages);
                    let first = {
                        let mut requests = model.requests.lock().unwrap();
                        requests.push(rendered);
                        requests.len() == 1
                    };
                    if first {
                        model.started.notify_one();
                        model.release.notified().await;
                    }
                    Ok(served::text(&request, "answered"))
                }
            })
            .build()
            .into_handle()
    }
}

/// With `DrainMode::All` installed, the inputs queued behind a running turn
/// are admitted together, as one run under the first one's id, whose
/// opening request renders each of them in order; the model is asked once
/// for them all.
async fn a_coalescing_drain_admits_the_queued_inputs_as_one_run(tier: Tier) {
    let model = Arc::new(GatedModel::default());
    let Some(world) = World::with_batching(
        tier,
        lash::QueuedWorkBatchingConfig::new(1).with_drain_mode(lash::DrainMode::All),
        model.provider(),
        |backend| lash::LashCore::standard_builder(backend.clone()),
    )
    .await
    else {
        return;
    };
    let session = world.session("coalescing-drain", served::spec(8)).await;
    tokio::time::timeout(WATCHDOG, async {
        let running = session
            .send(lash::TurnInput::text(BUSY))
            .await
            .expect("send the busy input");
        model.started.notified().await;
        let mut queued = Vec::new();
        for (index, text) in COALESCED.iter().enumerate() {
            let id = lash::TurnId::try_from(format!("coalesced-{index}")).expect("a host id");
            queued.push(
                session
                    .send(lash::TurnInput::text(*text))
                    .id(id)
                    .await
                    .expect("queue an input"),
            );
        }
        model.release.notify_one();
        running.output().await.expect("the busy run answers");
        let head = queued[0].id().expect("the host named the run").clone();
        for handle in queued {
            let input = handle.input_id().clone();
            let output = handle
                .output()
                .await
                .expect("every coalesced input settles");
            assert_eq!(output.status(), lash::TurnStatus::Answered);
            assert_eq!(
                session.attach(input).run().await.expect("read the binding"),
                Some(head.clone()),
                "every queued input is bound to the first one's run"
            );
        }
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "the queued inputs share one model call: {requests:?}"
        );
        let opening = &requests[1];
        let at: Vec<usize> = COALESCED
            .iter()
            .map(|text| opening.find(text).expect("the run renders every input"))
            .collect();
        assert!(
            at.windows(2).all(|pair| pair[0] < pair[1]),
            "the run renders its inputs in queue order: {opening}"
        );
    })
    .await
    .expect("deadlock watchdog: the coalesced run never settled");
    world.shutdown().await;
}

tiered_laws!(
    withdraw_while_queued_vs_cancel_while_running,
    cancelling_both_sends_stops_the_running_run_and_withdraws_the_queued_one,
    coalesced_inputs_commit_distinct_user_rows,
    host_input_ids_correlate_new_queued_and_steering_rows,
    a_coalescing_drain_admits_the_queued_inputs_as_one_run,
);

/// FIG-5288: a coalesced opening retains every input's own row and application,
/// in admission order, even though their answer belongs to one run.
async fn coalesced_inputs_commit_distinct_user_rows(tier: Tier) {
    let Some((stores, _keep)) = served::stores(tier).await else {
        return;
    };
    let backend = served::backend(stores);
    let scripts = Arc::new(served::Scripts::default());
    let build = |serve| {
        lash::LashCore::standard_builder(backend.clone())
            .serve_sessions(serve)
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .queued_work_batching(
                lash::QueuedWorkBatchingConfig::new(1).with_drain_mode(lash::DrainMode::All),
            )
            .serve_test_llm_profile(served::model(Arc::clone(&scripts)), served::metadata())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "input-rows",
                if serve { "serving" } else { "producer" },
            ))
            .expect("the core builds")
    };
    let producer = build(false);
    let session = producer
        .session(lash::SessionId::try_from("coalesced-input-rows".to_owned()).unwrap())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .expect("create the session");
    tokio::time::timeout(WATCHDOG, async {
        let names = [
            "first admitted input",
            "second admitted input",
            "third admitted input",
        ];
        let handles = session
            .send_batch(names.map(lash::TurnInput::text))
            .await
            .expect("accept the batch before its node starts");
        let expected: Vec<_> = handles
            .iter()
            .zip(names)
            .map(|(handle, text)| (handle.input_id().clone(), text.to_owned()))
            .collect();
        // The serving node currently drains one row per run. Record the
        // multi-input admission whose transcript contract this law pins.
        let run_id = handles[0].id().expect("the send names its run").clone();
        let ids = handles
            .iter()
            .map(|handle| handle.input_id().clone())
            .collect::<Vec<_>>();
        admit_composed_inputs(&backend, session.session_id(), &run_id, ids).await;
        let serving = build(true);
        let mut run = None;
        for handle in handles {
            let outcome = handle.outcome().await.expect("the coalesced run answers");
            served::assert_answered(
                "coalesced inputs",
                outcome.output().expect("a settled output"),
            );
            let this_run = outcome.run().expect("the run was admitted").clone();
            if let Some(run) = &run {
                assert_eq!(&this_run, run);
            }
            run = Some(this_run);
        }
        assert_input_rows(&session, &expected, &run.unwrap()).await;
        serving.shutdown().await.expect("stop the serving node");
    })
    .await
    .expect("deadlock watchdog: coalesced inputs never answered");
    producer.shutdown().await.expect("stop the producer");
}

/// Whether `entry` is a committed user message.
fn is_user_entry(entry: &lash::transcript::TranscriptEntry) -> bool {
    matches!(
        &entry.item,
        lash::transcript::TranscriptItem::Message(message)
            if message.role == lash::transcript::TranscriptRole::User
    )
}

/// A user message's text blocks, one per line.
fn user_text(entry: &lash::transcript::TranscriptEntry) -> String {
    let lash::transcript::TranscriptItem::Message(message) = &entry.item else {
        return String::new();
    };
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            lash::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every accepted input appears exactly once in committed history, with the
/// same id in its row and application, and with no neighbouring input's text.
async fn assert_input_rows(
    session: &lash::DurableSession,
    expected: &[(lash::InputId, String)],
    run: &lash::TurnId,
) {
    let transcript = session.transcript().await.expect("read committed rows");
    let rows: Vec<_> = transcript
        .visible()
        .filter(|row| is_user_entry(row))
        .collect();
    let actual: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.provenance
                    .input_id
                    .clone()
                    .expect("each user row names its input"),
                user_text(row),
            )
        })
        .collect();
    assert_eq!(
        actual, expected,
        "every admitted input has its own ordered row"
    );
    assert!(
        rows.iter()
            .all(|row| row.provenance.turn_id.as_ref() == Some(run))
    );
    let applications = session
        .turn_input_applications()
        .await
        .expect("read applications");
    assert_eq!(applications.len(), expected.len());
    assert_eq!(
        applications
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    );
    for application in &applications {
        assert_eq!(
            application.committed_message_id,
            lash_core_store::turn_input_vocabulary::ingress_message_id(
                application.input_id.as_str()
            )
        );
    }
    let message_ids: std::collections::HashSet<_> = applications
        .iter()
        .map(|application| &application.committed_message_id)
        .collect();
    assert_eq!(
        message_ids.len(),
        expected.len(),
        "each application names its own message"
    );
}

/// Record a composed admission, then release its actor for the serving node.
async fn admit_composed_inputs(
    backend: &lash::Backend,
    session: &lash::SessionId,
    run: &lash::TurnId,
    inputs: Vec<lash::InputId>,
) {
    use lash_core_store::store::{AdmittedInputIds, AdmittedTurnRows, RunAdmissionRecord};
    use lash_durable::domain::{DomainWrite, SessionMailWrite, TurnWrite};
    let database = backend.durable();
    let actor = lash_durable::ActorKey::session(session.as_str()).unwrap();
    let snapshot = database
        .actor(&actor)
        .await
        .unwrap()
        .expect("the inputs woke the session");
    let lease = database
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("composed-input-admission"),
            decodes: vec![snapshot.formats],
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = database.claim(&lease, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let mut tx = database.begin(&actor, claimed[0].epoch).await.unwrap();
    tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        inputs: inputs.clone(),
        batches: Vec::new(),
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Inputs {
                ids: AdmittedInputIds::new(inputs).unwrap(),
            },
            trace: None,
        },
        turn_deadline: None,
    }));
    tx.ack_seen();
    database
        .commit(tx, lash_durable::CommitLabel::TURN_ADMIT)
        .await
        .unwrap();
    database.release_node(&lease).await.unwrap();
}

/// FIG-5288: the id a host computes before sending is the accepted input's
/// id and the committed row's provenance, for new, queued and steering sends.
async fn host_input_ids_correlate_new_queued_and_steering_rows(tier: Tier) {
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = lash_core::testing::TestProvider::builder()
        .kind("input-correlation-model")
        .complete({
            let started = Arc::clone(&started);
            let gate = Arc::clone(&gate);
            move |request: lash_core::llm::types::LlmRequest| {
                let started = Arc::clone(&started);
                let gate = Arc::clone(&gate);
                let calls = Arc::clone(&calls);
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        started.notify_one();
                        gate.acquire()
                            .await
                            .expect("the host opens the model gate")
                            .forget();
                    }
                    Ok(served::text(&request, "done"))
                }
            }
        })
        .build()
        .into_handle();
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        lash::LashCore::standard_builder(backend.clone())
    })
    .await
    else {
        return;
    };
    let session = world
        .session("host-input-correlation", served::spec(8))
        .await;
    let live = world
        .core
        .session(session.session_id().clone())
        .open()
        .await
        .expect("open the facade");
    tokio::time::timeout(WATCHDOG, async {
        let ids = ["opening-send", "queued-send", "steering-send"]
            .map(|id| lash::TurnId::try_from(id.to_owned()).unwrap());
        let texts = ["opening input", "queued input", "steering input"];
        let expected: Vec<_> = ids
            .iter()
            .zip(texts)
            .map(|(id, text)| {
                let input = live.input_id(id);
                assert_eq!(
                    input,
                    session.input_id(id),
                    "both facades compute the same id"
                );
                (input, text.to_owned())
            })
            .collect();
        let first = live
            .send(lash::TurnInput::text(texts[0]))
            .id(ids[0].clone())
            .await
            .expect("accept the opening input");
        assert_eq!(first.input_id(), &expected[0].0);
        started.notified().await;
        let queued = session
            .send(lash::TurnInput::text(texts[1]))
            .id(ids[1].clone())
            .await
            .expect("accept the queued input");
        assert_eq!(queued.input_id(), &expected[1].0);
        assert_eq!(
            queued.run().await.unwrap(),
            None,
            "the model still holds the first run"
        );
        let steering = live
            .send(lash::TurnInput::text(texts[2]))
            .id(ids[2].clone())
            .ingress(lash::persistence::TurnInputIngress::active_turn(
                ids[0].clone(),
                Default::default(),
            ))
            .await
            .expect("accept the steering input");
        assert_eq!(steering.input_id(), &expected[2].0);
        gate.add_permits(1);
        for handle in [first, queued, steering] {
            served::assert_answered("host input correlation", &handle.output().await.unwrap());
        }
        let transcript = session.transcript().await.expect("read committed rows");
        let rows: Vec<_> = transcript
            .visible()
            .filter(|row| is_user_entry(row))
            .collect();
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.provenance
                        .input_id
                        .clone()
                        .expect("the user row names its input"),
                    user_text(row),
                ))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(rows[0].provenance.turn_id.as_ref(), Some(&ids[0]));
        assert_eq!(rows[1].provenance.turn_id.as_ref(), Some(&ids[1]));
    })
    .await
    .expect("deadlock watchdog: correlated inputs never answered");
    drop(live);
    world.shutdown().await;
}

/// ADR 0015/0033: the cancelled turn observes the sealed failed attempt
/// followed by the aborted attempt, even when it cancels in retry backoff.
#[tokio::test]
async fn sqlite_memory_cancelled_model_keeps_sealed_attempts() {
    #[derive(Debug, Default)]
    struct HeldBackoff(tokio::sync::Notify);

    #[async_trait::async_trait]
    impl lash_core::Clock for HeldBackoff {
        fn now(&self) -> std::time::Instant {
            lash_core::facade_support::SystemClock.now()
        }
        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            lash_core::facade_support::SystemClock.timestamp_datetime()
        }
        async fn sleep(&self, duration: std::time::Duration) {
            if duration == std::time::Duration::from_secs(7) {
                self.0.notify_one();
                std::future::pending().await
            } else {
                lash_core::facade_support::SystemClock.sleep(duration).await;
            }
        }
        async fn sleep_until(&self, deadline: std::time::Instant) {
            lash_core::facade_support::SystemClock
                .sleep_until(deadline)
                .await;
        }
    }

    let clock = Arc::new(HeldBackoff::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut options = lash_core::facade_support::ProviderOptions::default();
    options.reliability.retry.max_attempts = Some(2);
    options.reliability.retry.base_delay_ms = 7_000;
    options.reliability.retry.max_delay_ms = 7_000;
    options.reliability.retry.jitter_ms = 0;
    let provider = lash_core::testing::TestProvider::builder()
        .kind("cancel-backoff")
        .options(options)
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Err(lash_core::llm::transport::LlmTransportError::new("retry me")
                        .with_kind(lash_core::ProviderFailureKind::Transport)
                        .with_headers([("x-request-id", "failed-attempt")])
                        .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::RetryableTransient))
                }
            }
        }).build().into_handle();
    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
        .await
        .expect("memory stores");
    let backend = served::backend(Arc::new(stores));
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(provider, served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "cancel-backoff",
            "boot",
        ))
        .expect("core builds");
    let session = core
        .session(lash::SessionId::parse("cancel-model-backoff").unwrap())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .expect("session created");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let handle = session
            .send(lash::TurnInput::text("retry then cancel"))
            .await
            .expect("send input");
        clock.0.notified().await;
        assert!(matches!(
            handle.cancel().await.expect("cancel input"),
            lash::CancelReceipt::Cancelled { .. }
        ));
        let output = handle.output().await.expect("settled turn");
        assert_eq!(output.result.status(), lash::TurnStatus::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry was sent");
        let [call] = output.result.llm_calls.as_slice() else {
            panic!("one model call settles: {:?}", output.result.llm_calls);
        };
        assert_eq!(
            call.attempts.len(),
            2,
            "failed and cancelled attempts: {call:?}"
        );
        let failed = &call.attempts[0];
        assert_eq!(failed.ordinal, 1);
        assert_eq!(failed.outcome, lash_core::AttemptOutcome::Failed);
        assert_eq!(
            failed
                .error
                .as_ref()
                .unwrap()
                .provider_request_id
                .as_deref(),
            Some("failed-attempt")
        );
        assert!(matches!(
            failed.retry_decision,
            Some(lash_core::RetryDecision::Scheduled {
                wait: lash_core::RetryWait::Backoff,
                ..
            })
        ));
        let cancelled = &call.attempts[1];
        assert_eq!(cancelled.ordinal, 2);
        assert_eq!(cancelled.outcome, lash_core::AttemptOutcome::Aborted);
        assert_eq!(
            cancelled.error.as_ref().unwrap().code,
            Some(lash_core::FailureCode::lash(
                lash_core::TurnFailureCode::Cancelled
            ))
        );
    })
    .await
    .expect("cancellation settles without releasing the retry clock");
    core.shutdown().await.expect("core stops");
}
