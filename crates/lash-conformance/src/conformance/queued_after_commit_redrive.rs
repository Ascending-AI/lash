//! FIG-3748: a queued drive crashed after its first commit, with a second
//! input queued behind it, replays the committed root and then runs the
//! second input once.
//!
//! Two inputs are accepted before any drive runs, and each is its own root
//! (the turn-input claim bound is one). The first drive commits the first
//! root and its worker dies before the drive ended. The tier redrives it,
//! and the redrive replays its journal against a store that has moved on:
//! the first input is consumed and the second is the queue's head. A redrive
//! that re-decided from that live state would admit the second input where
//! its journal holds the first root's calls (on Restate, a journal mismatch
//! at the root's third call). The redrive must instead replay the recorded
//! admission and the committed root, asking the model nothing and committing
//! nothing again; the next drive then runs the second input exactly once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use crate::admit;

/// Panics as the first committed turn's delivery begins: its commit is
/// durable and the drive has not ended.
struct PanicAfterFirstCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterFirstCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the first queued root's commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, _phase: &str) {}
}

#[derive(Clone)]
struct RedriveParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: RedriveParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_store(parts.store)
            .build(),
    )
    .await
    .expect("build the queued after-commit redrive conformance runtime")
}

type DriveResultTx = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One attempt at a drive: the crashing one panics after its first commit;
/// every other sends back how its drive ended.
fn attempt(
    parts: &RedriveParts,
    crash: bool,
    result_tx: Option<DriveResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(PanicAfterFirstCommit));
            }
            let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(result_tx) = result_tx else {
                panic!(
                    "the crash probe did not fire after the first commit: {:?}",
                    drive.map(crate::facade_support::QueuedTurnDrain::ran)
                );
            };
            let end = crate::ConformanceTurnEnd::of(&drive);
            let _ = result_tx.send(drive);
            end
        })
    })
}

/// A queued drive crashed after its first commit and redriven replays that
/// root; the input queued behind it then runs once, in arrival order.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_queued_drive_redriven_after_its_first_commit_runs_the_next_input_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-queued-redrive-session"));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1).with_max_turn_input_claim(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let mut accepted = Vec::new();
    for text in ["first queued question", "second queued question"] {
        accepted.push(
            store
                .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                    session_id.clone(),
                    crate::TurnInputIngress::NextTurn,
                    crate::TurnInput::text(text),
                ))
                .await
                .expect("accept a queued input")
                .input_id,
        );
    }
    let before = store
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .map_or(0, |head| head.head_revision);
    let parts = RedriveParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
    };

    // The first drive crashes after its first commit and is redriven.
    let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::queue_drain(
                &session_id,
                format!("{prefix}-queued-redrive-1"),
            )),
            attempt(&parts, true, None),
            attempt(&parts, false, Some(first_tx)),
        ),
    )
    .await
    .expect("the redrive after the first commit ends (FIG-3748: it diverged from its journal)");
    first_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven drive")
        .unwrap_or_else(|error| panic!("the redrive after the first commit replays: {error:?}"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the redrive reads the first root's answer back and admits nothing it did not record"
    );
    let applied: Vec<_> = store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read the applied inputs")
        .into_iter()
        .map(|application| application.input_id)
        .collect();
    assert_eq!(
        applied,
        [accepted[0].clone()],
        "only the first input is applied by the redriven drive"
    );

    // The next drive runs the second input once.
    let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_turn(
            admit(crate::ExecutionScope::queue_drain(
                &session_id,
                format!("{prefix}-queued-redrive-2"),
            )),
            attempt(&parts, false, Some(second_tx)),
        ),
    )
    .await
    .expect("the second drive ends");
    let second = second_rx
        .recv()
        .await
        .expect("the tier's runner ran the second drive")
        .unwrap_or_else(|error| panic!("the second drive runs: {error:?}"))
        .ran()
        .expect("the second drive runs the second input");
    assert_eq!(second.assistant_output.safe_text, "answer 2");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "each input asks the model once"
    );
    let applied: Vec<_> = store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read the applied inputs")
        .into_iter()
        .map(|application| application.input_id)
        .collect();
    assert_eq!(
        applied, accepted,
        "both inputs are applied, in arrival order"
    );
    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("read the pending inputs");
    assert!(pending.is_empty(), "nothing stays queued: {pending:?}");
    let head = store
        .load_session_head_meta()
        .await
        .expect("read the head")
        .expect("both roots committed");
    assert!(
        head.head_revision > before,
        "the roots committed: {} -> {}",
        before,
        head.head_revision
    );
}

/// Register the queued after-commit redrive law (FIG-3748): a queued drive
/// crashed after its first commit replays that root and the input queued
/// behind it runs once. The fixture hands back a guard, a prefix, the tier's
/// effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! queued_after_commit_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::queued_after_commit_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_queued_drive_redriven_after_its_first_commit_runs_the_next_input_once, "queued-after-commit-redrive"));
    };
    (@law [$(#[$attr:meta])*] $fixture:block; ($law:ident, $label:literal)) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}
