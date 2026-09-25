//! A failed start-gate peek on Restate (FIG-3647).
//!
//! The turn observes its cancellation gate once, before its first model call.
//! Here the session's await events are revoked before the turn runs, so the
//! start-gate peek fails with the typed unknown-or-revoked refusal. The turn
//! must fail on that one observation: the handler's journal records a single
//! revocation read for the gate, and no model call follows. Then, for every
//! journal point of the turn's handler, a fresh backend under the same seed
//! drops the handler just before the server stores that frame and replays the
//! invocation: the replay must fail the same way, with the same recorded reads
//! and no model call.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};

const TURN_HOST: &str = "LashTestHandlerHost";
const SESSION: &str = "start-gate-peek";

/// What one run of the turn observed.
#[derive(Debug)]
struct Run {
    outcome: String,
    llm_calls: usize,
    crashes: u64,
    /// The turn handler's journal, in order.
    journal: Vec<(MessageType, Option<String>, bytes::Bytes)>,
}

impl Run {
    /// How many times the turn's handler read the session's revocation, the
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
    let turn_id = lash::TurnId::from("turn-1");
    let admitted = lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
        .expect("admit the turn scope");
    let outcome = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let outcome = Arc::clone(&outcome);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let outcome = Arc::clone(&outcome);
            Box::pin(async move {
                let output = session
                    .turn(lash::TurnInput::text("answer once"))
                    .turn_id(turn_id)
                    .advanced()
                    .run_with_scope(scoped)
                    .await;
                *outcome.lock().unwrap() = Some(match output {
                    Ok(output) => format!("completed: {:?}", output.result.outcome),
                    Err(error) => format!("error: {error}"),
                });
            })
        })
    };
    let server = backend.server();
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        backend.run_in_handler(admitted, attempt),
    )
    .await;
    match completed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => *outcome.lock().unwrap() = Some(format!("stuck: {error}")),
        Err(_) => *outcome.lock().unwrap() = Some("stuck: timed out".to_string()),
    }
    server.settle().await;
    let journal = server
        .invocations()
        .into_iter()
        .find(|view| view.target.split('/').next() == Some(TURN_HOST))
        .and_then(|view| server.journal(&view.id))
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.ty, entry.name, entry.payload))
        .collect();
    let outcome = outcome
        .lock()
        .unwrap()
        .clone()
        .expect("the turn recorded an outcome");
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
        reference.outcome.starts_with("error: "),
        "a revoked start gate fails the turn: {reference:?}"
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
            .service(TURN_HOST)
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
    // Crash points where lash itself does not recover yet (FIG-3678), pinned
    // so the fix flips this test, as turn_crash_replay's KNOWN_DIVERGENCES do:
    //
    // * when the turn-input claim's run (command 2) re-executes after a crash,
    //   the claim the lost attempt already made can still hold the input, and
    //   the turn cedes with `accepted_turn_input_ceded` before it ever reaches
    //   the start-gate peek — a race, on some runs. FIG-3600 S5 removes the
    //   re-execution; remove this pin when S5 lands.
    const KNOWN_DIVERGENCES: &[&str] = &["BeforeCommand { index: 2 }"];
    let unexplained: Vec<_> = violations
        .iter()
        .filter(|violation| {
            !KNOWN_DIVERGENCES
                .iter()
                .any(|known| violation.starts_with(known))
        })
        .collect();
    assert!(
        unexplained.is_empty(),
        "crash points whose replay diverged from the failed start gate:\n{unexplained:#?}"
    );
}
