//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A root resolves its session config once, as a recorded step at the top of
//! the logical-turn funnel, and every physical turn of the root runs under
//! that record. A redrive replays the record instead of reading the live
//! head, so a config change that landed after the root committed never
//! reaches the root's replay, and an input that arrives after the change
//! runs under it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// The model the session starts on.
const FIRST_MODEL: &str = "mock-model";
/// The model a config command moves the session to.
const SECOND_MODEL: &str = "turn-config-second-model";

/// Everything a runtime for these laws is built from, shared by every
/// attempt so each is the same session on the same store.
#[derive(Clone)]
struct ConfigParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: ConfigParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(crate::testing::test_standard_protocol_factories())
            .with_store(parts.store)
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the turn-config conformance runtime")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a literal model spec always builds"
)]
fn second_model() -> crate::ModelSpec {
    crate::ModelSpec::builder(SECOND_MODEL)
        .context_window_tokens(200_000)
        .build()
        .expect("the second model spec builds")
}

/// Move the session to [`SECOND_MODEL`] through the command lane.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the config command settles on a live store"
)]
async fn command_second_model(runtime: &mut crate::LashRuntime) {
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(second_model()),
            ..crate::SessionConfigPatch::default()
        })
        .await
        .expect("the model change settles through the command lane");
}

fn text_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
    input
}

/// A model that answers `answer <n>` to its n-th call and records the model
/// every call named.
fn recording_model(
    calls: &Arc<AtomicUsize>,
    models: &Arc<std::sync::Mutex<Vec<String>>>,
) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    let models = Arc::clone(models);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.model.clone());
            async move {
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: format!("answer {}", index + 1),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

type TurnResultTx =
    tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>;

/// A committed root redriven after a later model change replays under the
/// config it recorded at its start (D3 §2.2).
///
/// Root A commits on the first model. Before its reply reaches anyone, the
/// session's next boundary applies a model change, and the execution dies.
/// The tier redrives A: it must replay to its committed answer, its model
/// call must read back the first model's answer, it must not park as a
/// replay divergence, and it must leave the durable head on the second
/// model. A root that follows then runs on the second model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_root_redriven_after_a_model_change_replays_its_recorded_config(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-turn-config-replay-session"));
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        recording_model(&calls, &models),
    ));
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let parts = ConfigParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
    };
    let root = TurnId::from(format!("{prefix}-turn-config-replay-root"));
    let (result_tx, mut result_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<crate::AssembledTurn, crate::RuntimeError>>();

    // The first execution commits A, and then the model change lands at
    // the session's next boundary. The execution dies before A's reply
    // leaves it: the reply is lost.
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let root = root.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let root = root.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(parts).await;
                let turn = runtime
                    .stream_turn(
                        text_input(&root, "first question"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await
                    .unwrap_or_else(|error| panic!("root A commits on the first model: {error:?}"));
                assert!(
                    matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
                    "root A finishes on its first execution: {:?}",
                    turn.outcome
                );
                command_second_model(&mut runtime).await;
                panic!("injected loss of root A's reply after the model change landed");
            })
        })
    };
    let redrive: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let root = root.clone();
        let result_tx: TurnResultTx = result_tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let root = root.clone();
            let result_tx = result_tx.clone();
            Box::pin(async move {
                let mut runtime = build_runtime(parts).await;
                let turn = runtime
                    .stream_turn(
                        text_input(&root, "first question"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = result_tx.send(turn);
                end
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &root)),
            crashing,
            redrive,
        )
        .await;
    let committed = store
        .load_session_head_meta()
        .await
        .expect("read the head after the model change")
        .expect("root A's commit and the model change are durable");
    assert_eq!(
        committed.config.model.id, SECOND_MODEL,
        "precondition: the model change landed on the durable head before the redrive"
    );
    let revision_after_change = committed.head_revision;

    let turn = result_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven root")
        .unwrap_or_else(|error| {
            panic!("the redrive of committed root A replays under its recorded config: {error:?}")
        });
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redrive of root A finishes: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answer 1",
        "the redrive answers with what root A committed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the redrive reads root A's model call back instead of asking again"
    );
    assert!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the session's park")
            .is_none(),
        "the redrive of a committed root does not park as a replay divergence"
    );
    let head = store
        .load_session_head_meta()
        .await
        .expect("read the head after the redrive")
        .expect("the head is durable");
    assert_eq!(
        head.head_revision, revision_after_change,
        "the redrive commits nothing again"
    );
    assert_eq!(
        head.config.model.id, SECOND_MODEL,
        "the redrive leaves the model change on the durable head"
    );

    // A root that follows runs on the model the change moved the session to.
    let next = TurnId::from(format!("{prefix}-turn-config-replay-next"));
    let (next_tx, mut next_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(admit(crate::ExecutionScope::turn(&session_id, &next)), {
            let parts = parts.clone();
            let next = next.clone();
            Arc::new(move |scope| {
                let parts = parts.clone();
                let next = next.clone();
                let next_tx = next_tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let turn = runtime
                        .stream_turn(
                            text_input(&next, "second question"),
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await;
                    let end = crate::ConformanceTurnEnd::of(&turn);
                    let _ = next_tx.send(turn);
                    end
                })
            })
        })
        .await;
    let next_turn = next_rx
        .recv()
        .await
        .expect("the tier's runner ran the next root")
        .unwrap_or_else(|error| panic!("the next root runs: {error:?}"));
    assert!(
        matches!(next_turn.outcome, crate::TurnOutcome::Finished(_)),
        "the next root finishes: {:?}",
        next_turn.outcome
    );
    assert_eq!(
        models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
        "root A's one model call named the first model, and the next root's the second"
    );
}
