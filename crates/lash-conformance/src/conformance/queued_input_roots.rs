//! FIG-4457: under the default drain, every queued next-turn input is its
//! own root, and a cancel of one never reaches another.
//!
//! A host sends two next-turn inputs, each under its own host id. The first
//! is accepted while the session is idle; its drive's worker dies before
//! the drive admitted anything, and the second is accepted while that
//! worker is down. The tier redrives, and the redriven drive admits the
//! first input's root with both inputs open. The host's drain policy
//! chooses how much eligible work one root takes (ADR 0101 §5.2), and the
//! default takes one row: the first input's root takes the first input
//! alone. While that root runs, the host cancels the second input, which is
//! still open and is withdrawn; the first input's root answers it untouched,
//! and nothing ever runs the second.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use crate::admit;

const FIRST_TEXT: &str = "first queued input";
const SECOND_TEXT: &str = "second queued input";

#[derive(Clone)]
struct DriveParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: DriveParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .build(),
    )
    .await
    .expect("build the queued input roots conformance runtime")
}

type DriveResultTx = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One drive of the session's next queued root, which sends back how it
/// ended.
fn drive(parts: &DriveParts, result_tx: DriveResultTx) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let end = crate::ConformanceTurnEnd::of(&drive);
            let _ = result_tx.send(drive);
            end
        })
    })
}

/// The first drive's worker dies before it admitted anything, and the
/// second input is accepted while it is down.
fn die_after_the_second_send(parts: &DriveParts, second: String) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |_scope| {
        let parts = parts.clone();
        let second = second.clone();
        Box::pin(async move {
            parts
                .store
                .enqueue_pending_turn_input(
                    crate::PendingTurnInputDraft::new(
                        parts.session_id.clone(),
                        crate::TurnInputIngress::NextTurn,
                        crate::TurnInput::text(SECOND_TEXT),
                    )
                    .with_source_key(second),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("accept the second input while the worker is down: {error:?}")
                });
            panic!("injected worker death before the first input's drive admitted anything");
        })
    })
}

/// Two queued next-turn inputs, the second accepted while the first one's
/// drive is down, get their own roots under the default drain; a cancel of
/// the second while the first one's root runs withdraws the second alone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn two_queued_inputs_sent_across_a_restart_get_their_own_roots_and_a_cancel_of_one_leaves_the_other(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-queued-input-roots-session"));
    let first_key = format!("{prefix}-queued-input-roots-first");
    let second_key = format!("{prefix}-queued-input-roots-second");
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let rendered = Arc::new(Mutex::new(Vec::<String>::new()));
    let cancel = Arc::new(Mutex::new(None::<crate::PendingTurnInputCancelOutcome>));
    let calls = Arc::new(AtomicUsize::new(0));
    // The host cancels the second input as the first input's root asks the
    // model: the root is admitted and running.
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let rendered = Arc::clone(&rendered);
            let cancel = Arc::clone(&cancel);
            let calls = Arc::clone(&calls);
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let second_key = second_key.clone();
            move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let messages = serde_json::to_string(&request.messages)
                    .expect("a model request's messages serialize");
                rendered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(messages);
                let cancel = Arc::clone(&cancel);
                let store = Arc::clone(&store);
                let session_id = session_id.clone();
                let second_key = second_key.clone();
                async move {
                    if index == 0 {
                        let receipts = store
                            .cancel_pending_turn_inputs(
                                &session_id,
                                &[crate::PendingTurnInputCancelTarget::source_key(second_key)],
                            )
                            .await
                            .expect("cancel the second input while the first root runs");
                        *cancel
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            receipts.into_iter().next().map(|receipt| receipt.outcome);
                    }
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
    // The shipped batching: the default drain, one row at a time.
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.models = crate::testing::standard_test_models(model.into_handle());
    let first = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                session_id.clone(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text(FIRST_TEXT),
            )
            .with_source_key(first_key.clone()),
        )
        .await
        .expect("accept the first input")
        .input_id;
    let parts = DriveParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
    };

    let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &session_id,
                format!("{prefix}-queued-input-roots-1"),
            )),
            die_after_the_second_send(&parts, second_key.clone()),
            drive(&parts, first_tx),
        ),
    )
    .await
    .expect("the redriven drive ends");
    let answered = first_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven drive")
        .unwrap_or_else(|error| panic!("the redriven drive runs: {error:?}"))
        .ran()
        .expect("the redriven drive runs the first input's root");
    assert_eq!(answered.assistant_output.safe_text, "answer 1");

    let cancel = cancel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("the host cancelled the second input while the first root ran");
    assert!(
        matches!(
            &cancel,
            crate::PendingTurnInputCancelOutcome::Cancelled(input)
                if input.source_key.as_deref() == Some(second_key.as_str())
        ),
        "a cancel of the second input while the first input's root runs withdraws the second \
         input alone; it never names the first input's root (FIG-4457): {cancel:?}"
    );
    let rendered = rendered
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        rendered.len(),
        1,
        "the first input's root asks the model once"
    );
    assert!(
        rendered[0].contains(FIRST_TEXT) && !rendered[0].contains(SECOND_TEXT),
        "the first input's root renders the first input alone: {}",
        rendered[0]
    );
    let applied: Vec<_> = store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read the applied inputs")
        .into_iter()
        .map(|application| (application.input_id, application.turn_id))
        .collect();
    assert_eq!(
        applied,
        [(
            first,
            crate::store::PhysicalTurn::derive_turn_id(&crate::TurnId::from(first_key), 0),
        )],
        "the first input's own root applies it, untouched by the second input's cancel"
    );

    // Nothing is left for the second input: the next drive runs no root.
    let (next_tx, mut next_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_turn(
            admit(crate::ExecutionScope::turn(
                &session_id,
                format!("{prefix}-queued-input-roots-2"),
            )),
            drive(&parts, next_tx),
        ),
    )
    .await
    .expect("the next drive ends");
    let next = next_rx
        .recv()
        .await
        .expect("the tier's runner ran the next drive")
        .unwrap_or_else(|error| panic!("the next drive answers: {error:?}"));
    assert!(
        next.ran().is_none(),
        "the withdrawn second input never runs"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "only the first input ran");
    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("read the pending inputs");
    assert!(pending.is_empty(), "nothing stays queued: {pending:?}");
}

/// Register the queued input roots law (FIG-4457): two queued next-turn
/// inputs sent across a worker's death get their own roots under the default
/// drain, and a cancel of one leaves the other untouched. The fixture hands
/// back a guard, a prefix, the tier's effect host, the store set under test
/// and its [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! queued_input_roots_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::queued_input_roots_tests!(@law [$(#[$attr])*] $fixture;
            (two_queued_inputs_sent_across_a_restart_get_their_own_roots_and_a_cancel_of_one_leaves_the_other, "queued-input-roots"));
    };
    (@law [$(#[$attr:meta])*] $fixture:block; ($law:ident, $label:literal)) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}
