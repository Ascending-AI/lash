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
use lash_core::{SessionId, SessionStoreFactory as _, StoreSet as _};
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
    lash_core::drive::ingress_drive_request(input_id.as_str())
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
        .send(lash::TurnInput::text("hello"))
        .await
        .expect("accept")
        .receipt()
        .clone();
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
        .send(lash::TurnInput::text("first question"))
        .await
        .expect("accept the first")
        .receipt()
        .clone();
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    let second = session
        .send(lash::TurnInput::text("second question"))
        .await
        .expect("accept the second")
        .receipt()
        .clone();
    let third = session
        .send(lash::TurnInput::text("third question"))
        .await
        .expect("accept the third")
        .receipt()
        .clone();
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

/// The double's drive hold: while a test holds the engine's drive of a
/// session, an input accepted there stays pending — no admission takes it —
/// and a root admitted before the hold still runs to its answer. Releasing
/// the hold lets the drive admit the input. This is how a law asserts what
/// is still pending without driving anything itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_drive_admits_nothing_until_released() {
    let world = world(0x5507).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world.core.session("held-drive").open().await.expect("open");
    let session_id = SessionId::from("held-drive");
    let running = session
        .send(lash::TurnInput::text("first question"))
        .await
        .expect("accept the first");
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    let hold = world.backend.hold_session_drive(&session_id).await;
    let held = session
        .send(lash::TurnInput::text("held question"))
        .await
        .expect("accept the held input");
    world.gate.release.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(20), running.outcome())
        .await
        .expect("the first root answers under the hold")
        .expect("the first outcome");
    assert!(
        matches!(first.status, lash::TurnStatus::Answered),
        "the admitted root runs on: {:?}",
        first.status
    );
    world.backend.server().settle().await;
    let pending = session
        .durable()
        .pending_turn_inputs()
        .await
        .expect("pending");
    assert!(
        pending.iter().any(|read| {
            read.input.input_id == held.receipt().input_id
                && matches!(read.status, lash::PendingTurnInputReadStatus::Pending)
        }),
        "the held drive admitted nothing: {pending:?}"
    );
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);

    hold.release();
    let second = tokio::time::timeout(Duration::from_secs(20), held.outcome())
        .await
        .expect("the released drive answers the held input")
        .expect("the held outcome");
    assert!(
        matches!(second.status, lash::TurnStatus::Answered),
        "{:?}",
        second.status
    );
    assert_eq!(world.calls.load(Ordering::SeqCst), 2);
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending after release")
            .is_empty()
    );
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
        .send(lash::TurnInput::text("keep me running"))
        .await
        .expect("accept the running input")
        .receipt()
        .clone();
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    let queued = session
        .send(lash::TurnInput::text("withdraw me"))
        .await
        .expect("accept the queued input")
        .receipt()
        .clone();

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
            .send(lash::TurnInput::text("finish without me"))
            .await
            .expect("accept")
            .receipt()
            .clone();
        receipt.input_id
    };
    let outcome = attach(&world.backend, &session_id, request_of(&input_id)).await;
    assert_eq!(answers(&outcome), ["answer 1"]);
    assert_eq!(world.calls.load(Ordering::SeqCst), 1);
}

/// L-S11: a row committed whose immediate delivery was lost (its process
/// died between the commit and the ask) is driven by the relay pass of the
/// engine's reconcile tick, through the ingress obligation its commit armed
/// (ADR 0109 §3).
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
    // The row commits through the store alone: its obligation is armed, but
    // no acceptance ran, so no drive was ever asked for.
    let store = world
        .backend
        .stores()
        .session_store_factory()
        .open_existing_store_by_id(&session_id)
        .await
        .expect("open the session store")
        .expect("the session exists");
    let input_id = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            lash::TurnInput::text("nobody asked for me"),
        ))
        .await
        .expect("commit the row")
        .input_id;
    // The engine driver's own reconcile tick (ADR 0104 O2) relays the due
    // obligation: its drive drains the row.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if session
                .durable()
                .pending_turn_inputs()
                .await
                .expect("pending")
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the reconcile tick's drive drains the row");
    #[derive(Debug, serde::Deserialize)]
    struct DriveAsk {
        idempotency_key: Option<String>,
    }
    let asks = lash_restate::RestateAdminClient::new(world.backend.connection())
        .query_json::<DriveAsk>(
            "SELECT idempotency_key FROM sys_invocation \
             WHERE target_service_name = 'LashSession' AND target_service_key = 'dropped-schedule'",
        )
        .await
        .expect("sys_invocation");
    let ask = lash_core::drive::ingress_drive_request(input_id.as_str());
    assert!(
        asks.iter()
            .any(|row| row.idempotency_key.as_deref() == Some(ask.as_str())),
        "the relay asked for the row's own drive: {asks:?}"
    );

    // The delivered obligation is settled: nothing asks for the row again.
    let ledger = world
        .backend
        .stores()
        .obligation_ledger(lash_core::store::ObligationKind::Ingress);
    assert_eq!(
        ledger
            .state(&lash_core::store::ingress_obligation::ingress_obligation_id(input_id.as_str()))
            .await
            .expect("obligation state"),
        Some(lash_core::store::ObligationState::Delivered)
    );
}

/// A row that lands while an engine-side invocation owns its session is
/// asked for once, through its own ingress obligation, and never by a sweep:
/// its drive queues behind the live one on the session's object, so no
/// sibling ever fences the live turn (S5a review, ADR 0109 §3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_with_live_engine_work_is_not_re_asked() {
    let world = world(0x5e55).await;
    world.gate.armed.store(true, Ordering::SeqCst);
    let session = world.core.session("in-flight").open().await.expect("open");
    let session_id = SessionId::from("in-flight");
    let receipt = session
        .durable()
        .send(lash::TurnInput::text("hold the model"))
        .await
        .expect("accept");
    tokio::time::timeout(Duration::from_secs(20), world.gate.reached.notified())
        .await
        .expect("the first turn is running");
    // A row lands with no ask of its own — lost-ask-shaped ingress while
    // the session's turn invocation is live on the engine.
    let store = world
        .backend
        .stores()
        .session_store_factory()
        .open_existing_store_by_id(&session_id)
        .await
        .expect("open the session store")
        .expect("the session exists");
    let behind = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            lash::TurnInput::text("queued behind the live turn"),
        ))
        .await
        .expect("commit the row")
        .input_id;

    // The engine driver's own reconcile tick (ADR 0104 O2) relays the row's
    // obligation. A full tick after the row lands finds no sweep ask and
    // exactly the row's own.
    tokio::time::sleep(Duration::from_secs(11)).await;
    #[derive(Debug, serde::Deserialize)]
    struct DriveAsk {
        idempotency_key: Option<String>,
    }
    let asks = lash_restate::RestateAdminClient::new(world.backend.connection())
        .query_json::<DriveAsk>(
            "SELECT idempotency_key FROM sys_invocation \
             WHERE target_service_name = 'LashSession' AND target_service_key = 'in-flight'",
        )
        .await
        .expect("sys_invocation");
    let own = lash_core::drive::ingress_drive_request(behind.as_str());
    assert!(
        asks.iter().all(|row| row
            .idempotency_key
            .as_deref()
            .is_none_or(|key| key.starts_with("ingress:"))),
        "only ingress obligations asked for drives: {asks:?}"
    );
    assert_eq!(
        asks.iter()
            .filter(|row| row.idempotency_key.as_deref() == Some(own.as_str()))
            .count(),
        1,
        "the row's own drive was asked for once: {asks:?}"
    );

    // The live drive's own re-admission picks the row up once the turn
    // ends: no sibling ever ran, and the work is not stranded.
    world.gate.release.notify_one();
    let outcome = attach(&world.backend, &session_id, request_of(receipt.input_id())).await;
    assert_eq!(answers(&outcome), ["answer 1", "answer 2"]);
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending")
            .is_empty()
    );
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
