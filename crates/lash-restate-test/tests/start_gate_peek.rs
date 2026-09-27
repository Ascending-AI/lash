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
    /// read the start-gate peek fails on.
    fn revocation_reads(&self) -> usize {
        self.journal
            .iter()
            .filter(|(ty, _, payload)| {
                ty.is_command()
                    && payload
                        .windows(b"is_revoked".len())
                        .any(|window| window == b"is_revoked")
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
        reference.outcome
            == r#"drive ran [Released { root: TurnId("turn-1") }], stopped RootAborted { root: TurnId("turn-1") }"#,
        "a revoked start gate fails the root, and the drive stops on it: {reference:?}"
    );
    assert_eq!(
        reference.llm_calls, 0,
        "no model call follows the failed gate"
    );
    // Two reads, and only one of them is the start gate: the failed turn's
    // teardown probes the gate once more to decide whether to repair its
    // orphaned inputs, and reads a revoked gate as nothing to repair. A retry
    // of the start gate would add a read per attempt.
    assert_eq!(
        reference.revocation_reads(),
        2,
        "the start gate is observed once, then the teardown probes it: {} {:?}",
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
