//! The S5 functional laws on the double (FIG-3600): what a session's
//! accepted input gets from the engine's drive.
//!
//! Every input enters through the session's durable ingress, and the
//! acceptance asks the engine for a drive; nothing here runs a turn itself.
//! The engine's `LashSession` drive admits the input and runs its root in a
//! `LashTurn` workflow, on the kernel drive the core installed. The laws
//! read the outcome from the drive and from durable session state, never
//! from a caller-held future.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::engine::{DriveOutcome, DriveRequestId, DriveStop, RootOutcome};
use lash_core::facade_support::{TurnFinish, TurnOutcome, TurnStop};
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::{SessionId, SessionStoreFactory as _, SessionWorkEngine};
use lash_restate_test::{RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE};

/// The first model call waits here until the law releases it, so a law can
/// act while a turn is running.
#[derive(Default)]
struct ModelGate {
    armed: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct World {
    backend: RestateTestBackend,
    core: lash::LashCore,
    calls: Arc<AtomicUsize>,
    gate: Arc<ModelGate>,
}

fn text(text: impl Into<String>) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.into(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..Default::default()
    }
}

fn build_core(
    backend: &RestateTestBackend,
    provider: lash_core::facade_support::ProviderHandle,
    owner: &str,
) -> lash::LashCore {
    lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(
            lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            owner,
        ))
        .expect("build the lash core")
}

async fn world(seed: u64) -> World {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(ModelGate::default());
    let provider = {
        let calls = Arc::clone(&calls);
        let gate = Arc::clone(&gate);
        lash_core::testing::TestProvider::builder()
            .kind("session-send")
            .complete(move |_request: LlmRequest| {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                let gate = Arc::clone(&gate);
                async move {
                    if call == 1 && gate.armed.load(Ordering::SeqCst) {
                        gate.reached.notify_one();
                        gate.release.notified().await;
                    }
                    Ok::<_, LlmTransportError>(text(format!("answer {call}")))
                }
            })
            .build()
            .into_handle()
    };
    let core = build_core(&backend, provider, "session-send");
    World {
        backend,
        core,
        calls,
        gate,
    }
}

/// The drive the acceptance of `input_id` scheduled.
fn request_of(input_id: &lash_core::InputId) -> DriveRequestId {
    DriveRequestId::new(input_id.to_string())
}

async fn attach(
    backend: &RestateTestBackend,
    session: &SessionId,
    request: DriveRequestId,
) -> DriveOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        backend.attach_drive(session, request),
    )
    .await
    .expect("the drive ends")
    .expect("the drive's outcome")
}

fn answers(outcome: &DriveOutcome) -> Vec<String> {
    outcome
        .ran
        .iter()
        .map(|root| match root {
            RootOutcome::Committed {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage { text }),
                ..
            } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// The session's messages, encoded as JSON, for ordering checks.
async fn transcript(session: &lash::LashSession) -> String {
    let view = session
        .durable()
        .read()
        .await
        .expect("read the session")
        .expect("the session exists");
    serde_json::to_string(view.messages()).expect("encode the transcript")
}

/// An input accepted on an idle session is admitted by the drive its
/// acceptance scheduled, at once: one root, answered and committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_send_is_claimed_at_once() {
    let world = world(0x5501).await;
    let session = world.core.session("idle-send").open().await.expect("open");
    let session_id = SessionId::from("idle-send");
    let receipt = session
        .durable()
        .enqueue(lash::TurnInput::text("hello"))
        .send()
        .await
        .expect("accept");
    let outcome = attach(&world.backend, &session_id, request_of(&receipt.input_id)).await;
    assert_eq!(answers(&outcome), ["answer 1"]);
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending")
            .is_empty()
    );
    let applied = session
        .durable()
        .turn_input_applications()
        .await
        .expect("applications");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].input_id, receipt.input_id);
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);
}

/// Inputs accepted while a turn runs are answered after it, in arrival
/// order: a send committed during a running drive is admitted by that drive's
/// next admission or by the drive its own schedule queued behind it, never
/// stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_sends_answer_in_arrival_order() {
    let world = world(0x5502).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world.core.session("busy-send").open().await.expect("open");
    let session_id = SessionId::from("busy-send");
    let first = session
        .durable()
        .enqueue(lash::TurnInput::text("first question"))
        .send()
        .await
        .expect("accept the first");
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    let second = session
        .durable()
        .enqueue(lash::TurnInput::text("second question"))
        .send()
        .await
        .expect("accept the second");
    let third = session
        .durable()
        .enqueue(lash::TurnInput::text("third question"))
        .send()
        .await
        .expect("accept the third");
    world.gate.release.notify_one();
    for receipt in [&first, &second, &third] {
        attach(&world.backend, &session_id, request_of(&receipt.input_id)).await;
    }
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending")
            .is_empty(),
        "every accepted input was driven"
    );
    let applied: Vec<_> = session
        .durable()
        .turn_input_applications()
        .await
        .expect("applications")
        .into_iter()
        .map(|application| application.input_id)
        .collect();
    assert_eq!(
        applied,
        [
            first.input_id.clone(),
            second.input_id.clone(),
            third.input_id.clone()
        ],
        "inputs are applied in arrival order"
    );
    let transcript = transcript(&session).await;
    let at = |needle: &str| {
        transcript
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is in the transcript: {transcript}"))
    };
    assert!(at("first question") < at("answer 1"));
    assert!(at("answer 1") < at("second question"));
    assert!(at("second question") < at("third question"));
}

/// An input withdrawn while it is still queued never runs; a turn cancelled
/// while it runs stops, cancelled, and commits that stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn withdraw_while_queued_vs_cancel_while_running() {
    let world = world(0x5503).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world
        .core
        .session("withdraw-cancel")
        .open()
        .await
        .expect("open");
    let session_id = SessionId::from("withdraw-cancel");
    let running = session
        .durable()
        .enqueue(lash::TurnInput::text("keep me running"))
        .send()
        .await
        .expect("accept the running input");
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    let queued = session
        .durable()
        .enqueue(lash::TurnInput::text("withdraw me"))
        .send()
        .await
        .expect("accept the queued input");

    let withdrawn = session
        .durable()
        .cancel_pending_turn_input(&queued.input_id)
        .await
        .expect("withdraw the queued input");
    assert!(
        matches!(
            withdrawn,
            lash_core::PendingTurnInputCancelOutcome::Cancelled(_)
        ),
        "a queued input withdraws: {withdrawn:?}"
    );

    let root = world
        .backend
        .server()
        .invocations()
        .into_iter()
        .find_map(|view| {
            view.target
                .strip_prefix(&format!(
                    "{TURN_DRIVER_SERVICE}/{}:{}",
                    session_id.as_str().len(),
                    session_id.as_str()
                ))
                .and_then(|rest| rest.strip_suffix("/run"))
                .map(str::to_owned)
        })
        .expect("the running root has its LashTurn");
    world
        .backend
        .restate()
        .turn_work_driver()
        .request_cancel(lash::TurnCancelRequest::new(
            lash::TurnAddress::new(session_id.as_str(), root.as_str()),
            "withdraw-cancel-stop",
            Some("user".to_owned()),
        ))
        .await
        .expect("request the running turn's cancel");
    world.gate.release.notify_one();

    let outcome = attach(&world.backend, &session_id, request_of(&running.input_id)).await;
    match outcome.ran.as_slice() {
        [
            RootOutcome::Committed {
                outcome: TurnOutcome::Stopped(TurnStop::Cancelled { .. }),
                ..
            },
        ] => {}
        other => panic!("the running turn stops cancelled: {other:?}"),
    }
    attach(&world.backend, &session_id, request_of(&queued.input_id)).await;
    let applied: Vec<_> = session
        .durable()
        .turn_input_applications()
        .await
        .expect("applications")
        .into_iter()
        .map(|application| application.input_id)
        .collect();
    assert!(
        !applied.contains(&queued.input_id),
        "the withdrawn input never runs: {applied:?}"
    );
    assert!(
        !transcript(&session).await.contains("withdraw me"),
        "the withdrawn input never reaches the transcript"
    );
}

/// The caller that accepted an input holds nothing the turn needs: dropping
/// the session handle it accepted through, and the acceptance itself, stops
/// nothing. The worker's core keeps serving drives, and the drive commits the
/// turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_handle_stops_nothing() {
    let world = world(0x5504).await;
    let session_id = SessionId::from("dropped-handle");
    let input_id = {
        let session = world
            .core
            .session("dropped-handle")
            .open()
            .await
            .expect("open");
        let receipt = session
            .durable()
            .enqueue(lash::TurnInput::text("finish without me"))
            .send()
            .await
            .expect("accept");
        receipt.input_id
    };
    let outcome = attach(&world.backend, &session_id, request_of(&input_id)).await;
    assert_eq!(answers(&outcome), ["answer 1"]);
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);
}

/// L-S11: a row committed whose schedule was lost (its process died between
/// the commit and the ask) is driven by the next reconcile sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_schedule_is_reconciled() {
    let world = world(0x5505).await;
    let session = world
        .core
        .session("dropped-schedule")
        .open()
        .await
        .expect("open");
    let session_id = SessionId::from("dropped-schedule");
    // The row commits through the store alone: no acceptance ran, so no
    // drive was ever asked for.
    let store = world
        .backend
        .stores()
        .session_store_factory()
        .open_existing_store_by_id(&session_id)
        .await
        .expect("open the session store")
        .expect("the session exists");
    let row = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            lash::TurnInput::text("nobody asked for me"),
        ))
        .await
        .expect("commit the row");
    world.backend.server().settle().await;
    assert!(
        world.backend.server().invocations().iter().all(|view| !view
            .target
            .starts_with(lash_restate_test::SESSION_DRIVER_SERVICE)),
        "no drive runs for a row nobody asked about"
    );

    let engine = Arc::clone(world.backend.restate().session_work_engine());
    let report = lash_core::drive::reconcile_session_work(
        world.backend.stores().session_store_factory().as_ref(),
        engine.as_ref() as &dyn SessionWorkEngine,
        "boot-1",
    )
    .await
    .expect("sweep");
    assert_eq!(report.scheduled, std::slice::from_ref(&session_id));
    let outcome = attach(
        &world.backend,
        &session_id,
        lash_core::drive::reconcile_drive_request("boot-1", &format!("input:{}", row.input_id)),
    )
    .await;
    assert_eq!(answers(&outcome), ["answer 1"]);
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending")
            .is_empty()
    );
    // A second sweep finds nothing open and asks for nothing.
    let again = lash_core::drive::reconcile_session_work(
        world.backend.stores().session_store_factory().as_ref(),
        engine.as_ref() as &dyn SessionWorkEngine,
        "boot-2",
    )
    .await
    .expect("sweep again");
    assert!(again.scheduled.is_empty());
}

/// LOW-14 (#2290 review): one engine serves one session driver, so a second
/// core built over the same backend does not drive its own sessions. The
/// build says so, naming the core whose driver is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_core_over_one_engine_reports_its_ignored_driver() {
    let world = world(0x5a14).await;
    let provider = lash_core::testing::TestProvider::builder()
        .kind("second-core")
        .complete(|_request: LlmRequest| async { Ok::<_, LlmTransportError>(text("second")) })
        .build()
        .into_handle();
    let (second, capture) = lash_core::testing::trace_capture::capturing(|| async {
        build_core(&world.backend, provider, "second-core")
    })
    .await;
    let ignored = capture.exactly_one("session_driver.install_ignored");
    assert_eq!(ignored.level, "WARN");
    assert_eq!(ignored.field("incarnation_id"), "second-core");
    drop(second);
    drop(world);
}
