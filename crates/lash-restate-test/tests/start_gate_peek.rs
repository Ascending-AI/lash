//! A failed start-gate peek on Restate (FIG-3647).
//!
//! The turn observes its cancellation gate once, before its first model call.
//! Here the session's await events are revoked before the turn runs, so the
//! start-gate peek fails with the typed unknown-or-revoked refusal. The turn
//! is sent to the session and the engine drives it: the root's `LashTurn`
//! workflow must fail on that one observation, its journal recording a single
//! revocation read for the gate, and no model call follows. Then, for every
//! journal point of the root's workflow, a fresh backend under the same seed
//! drops the handler just before the server stores that frame and replays the
//! invocation: the replay must fail the same way, with the same recorded reads
//! and no model call.
//!
//! An answered turn, beside it, reads its gate in one shared index call per
//! peek and publishes its terminal one-way (FIG-3978).

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    CrashPoint, CrashRule, RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE,
};

const SESSION: &str = "start-gate-peek";

/// What one run of the turn observed.
#[derive(Debug)]
struct Run {
    outcome: String,
    llm_calls: usize,
    crashes: u64,
    /// The root workflow's journal, in order.
    journal: Vec<(MessageType, Option<String>, bytes::Bytes)>,
}

impl Run {
    /// How many times the root's workflow read the session's revocation, the
    /// read the start-gate peek fails on: the gate's own `peek_turn_gate`,
    /// which answers the revocation with the gate (FIG-3978), or a general
    /// peek's `is_revoked`.
    fn revocation_reads(&self) -> usize {
        self.journal
            .iter()
            .filter(|(ty, _, payload)| {
                ty.is_command()
                    && [b"is_revoked".as_slice(), b"peek_turn_gate".as_slice()]
                        .iter()
                        .any(|handler| {
                            payload
                                .windows(handler.len())
                                .any(|window| window == *handler)
                        })
            })
            .count()
    }
}

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "start-gate-peek")
}

async fn run_turn(seed: u64, crash: Option<CrashRule>) -> Run {
    let backend: RestateTestBackend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let provider = {
        let llm_calls = Arc::clone(&llm_calls);
        lash_core::testing::TestProvider::builder()
            .kind("start-gate-peek")
            .complete(move |_request: LlmRequest| {
                llm_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, LlmTransportError>(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "answered".into(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let core =
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
            .build(owner())
            .expect("build the lash core");
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    // The session's await events are gone before the turn starts: its
    // start-gate peek has nothing to read.
    backend
        .lash_backend()
        .effect_host()
        .revoke_await_events_for_session(&lash_core::SessionId::from(SESSION))
        .await
        .expect("revoke the session's await events");
    if let Some(rule) = crash {
        backend.server().crash_on(rule);
    }
    let handle = session
        .send(lash::TurnInput::text("answer once"))
        .id("turn-1")
        .await
        .expect("accept the turn input");
    let request = lash_core::drive::ingress_drive_request(
        handle.input_id().as_str(),
        lash_core::drive::FIRST_INGRESS_ATTEMPT,
    );
    let server = backend.server();
    let drive = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        backend.attach_drive(&lash_core::SessionId::from(SESSION), request),
    )
    .await;
    let outcome = match drive {
        Ok(Ok(outcome)) => format!("drive ran {:?}, stopped {:?}", outcome.ran, outcome.stop),
        Ok(Err(error)) => format!("error: {error}"),
        Err(_) => "stuck: timed out".to_string(),
    };
    server.settle().await;
    let journal = server
        .invocations()
        .into_iter()
        .find(|view| view.target.split('/').next() == Some(TURN_DRIVER_SERVICE))
        .and_then(|view| server.journal(&view.id))
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.ty, entry.name, entry.payload))
        .collect();
    Run {
        outcome,
        llm_calls: llm_calls.load(Ordering::SeqCst),
        crashes: server.stats().crashes,
        journal,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_start_gate_peek_fails_the_turn_once_and_replays_identically() {
    let seed = 0x3647;
    let reference = run_turn(seed, None).await;
    assert!(
        reference.outcome == r#"drive ran [Released { root: TurnId("turn-1") }], stopped Idle"#,
        "a revoked start gate fails the root, whose refusal ends it, and the drive finds \
         nothing more to run: {reference:?}"
    );
    assert_eq!(
        reference.llm_calls, 0,
        "no model call follows the failed gate"
    );
    // One read: the start gate is observed once, and nothing probes it after
    // the failure. A failed root's rows are released by its terminal write,
    // not by a teardown repair that reads the gate (FIG-3927 §2.6). A retry
    // of the start gate would add a read per attempt.
    assert_eq!(
        reference.revocation_reads(),
        1,
        "the start gate is observed once and nothing probes it after the failure: {} {:?}",
        reference.outcome,
        reference.journal
    );
    let commands = reference
        .journal
        .iter()
        .filter(|(ty, _, _)| ty.is_command())
        .count();
    let mut violations = Vec::new();
    for index in 1..commands {
        let rule = CrashRule::new(CrashPoint::BeforeCommand { index })
            .service(TURN_DRIVER_SERVICE)
            .within_attempts(1);
        let run = run_turn(seed, Some(rule)).await;
        if run.crashes != 1
            || run.outcome != reference.outcome
            || run.llm_calls != 0
            || run.revocation_reads() != reference.revocation_reads()
        {
            violations.push(format!(
                "BeforeCommand {{ index: {index} }}: crashes={} outcome={:?} llm_calls={} reads={}",
                run.crashes,
                run.outcome,
                run.llm_calls,
                run.revocation_reads()
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "crash points whose replay diverged from the failed start gate:\n{violations:#?}"
    );
}

/// Whether `payload` names `needle` anywhere.
fn names(payload: &[u8], needle: &[u8]) -> bool {
    payload.windows(needle.len()).any(|window| window == needle)
}

/// An answered turn's gate peeks and terminal publication make no nested
/// index invocation (FIG-3978). Each peek of the cancellation gate — the start
/// gate and the gate after the model call — is one shared `peek_turn_gate`
/// read of the session's index, where it was an exclusive `is_revoked` call
/// on the index followed by a `peek` of the gate's workflow. The committed
/// terminal is sent to the index one-way, so the turn waits for neither the
/// index nor the workflow write behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answered_turn_peeks_its_gate_in_one_shared_read_and_publishes_one_way() {
    let backend: RestateTestBackend = lash_restate_test::backend(0x3978, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let provider = lash_core::testing::TestProvider::builder()
        .kind("gate-hops")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "answered".into(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let core =
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
            .build(owner())
            .expect("build the lash core");
    let session = core
        .session("gate-hops")
        .open()
        .await
        .expect("open the session");
    let handle = session
        .send(lash::TurnInput::text("answer once"))
        .id("turn-1")
        .await
        .expect("accept the turn input");
    let request = lash_core::drive::ingress_drive_request(
        handle.input_id().as_str(),
        lash_core::drive::FIRST_INGRESS_ATTEMPT,
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.attach_drive(&lash_core::SessionId::from("gate-hops"), request),
    )
    .await
    .expect("the drive ends")
    .expect("the drive's outcome");
    assert_eq!(outcome.ran.len(), 1, "one root ran: {outcome:?}");
    let server = backend.server();
    server.settle().await;

    let invocations = server.invocations();
    let mut by_handler = std::collections::BTreeMap::<String, usize>::new();
    for view in &invocations {
        let mut parts = view.target.split('/');
        let service = parts.next().unwrap_or_default();
        let handler = parts.next_back().unwrap_or_default();
        *by_handler
            .entry(format!("{service}/{handler}"))
            .or_default() += 1;
    }
    eprintln!(
        "FIG-3978 invocations for one answered turn: {} total, {by_handler:?}",
        invocations.len()
    );
    let turn = invocations
        .iter()
        .find(|view| view.target.split('/').next() == Some(TURN_DRIVER_SERVICE))
        .expect("the root's workflow ran");
    let journal = server.journal(&turn.id).expect("the root's journal");
    let calls_naming = |service: &[u8], handler: &[u8]| {
        journal
            .iter()
            .filter(|entry| {
                entry.ty == MessageType::CallCommand
                    && names(&entry.payload, service)
                    && names(&entry.payload, handler)
            })
            .count()
    };
    assert_eq!(
        calls_naming(b"LashDurableWaitIndex", b"peek_turn_gate"),
        3,
        "the start gate, the post-model gate and the teardown probe each read the index once: \
         {by_handler:?}"
    );
    assert_eq!(
        calls_naming(b"LashDurableWaitIndex", b"is_revoked")
            + calls_naming(b"LashDurableWaitWorkflow", b"peek"),
        0,
        "no peek of the turn's gate reads the revocation and the gate's workflow separately: \
         {by_handler:?}"
    );
    let index_resolves = |ty: MessageType| {
        journal
            .iter()
            .filter(|entry| {
                entry.ty == ty
                    && names(&entry.payload, b"LashDurableWaitIndex")
                    && names(&entry.payload, b"resolve")
            })
            .count()
    };
    assert_eq!(
        index_resolves(MessageType::OneWayCallCommand),
        1,
        "the committed terminal is published to the index one-way"
    );
    assert_eq!(
        index_resolves(MessageType::CallCommand),
        1,
        "only the gate's settlement before commit waits for the index"
    );
}
