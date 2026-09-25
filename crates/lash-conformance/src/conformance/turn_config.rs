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
    /// Plugins the law adds to the standard protocol.
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
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
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain(parts.tools)
                    .collect(),
            )
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
        tools: Vec::new(),
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

/// The parts of a turn-config law's session: a host whose resolver serves
/// the recording model, and the session's store.
async fn law_session(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    resolver: Arc<dyn lash_core::provider::RuntimeProviderResolver>,
) -> ConfigParts {
    let session_id = SessionId::from(format!("{prefix}-turn-config-{name}-session"));
    let mut host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver = resolver;
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    ConfigParts {
        session_id,
        host,
        store,
        tools: Vec::new(),
    }
}

/// What one turn attempt does before it sends its input.
#[derive(Clone, Copy)]
enum BeforeSend {
    Nothing,
    /// Move the session to [`SECOND_MODEL`] through the command lane first.
    CommandSecondModel,
}

/// Run `root` with `text` as one attempt on the tier's runner and hand back
/// how its turn returned.
async fn run_text_turn(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    root: &TurnId,
    text: &'static str,
    before: BeforeSend,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, root)),
            text_attempt(parts, root, text, before, turn_tx),
        )
        .await;
    turn_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran root `{root}`"))
}

fn text_attempt(
    parts: &ConfigParts,
    root: &TurnId,
    text: &'static str,
    before: BeforeSend,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let root = root.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let root = root.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if matches!(before, BeforeSend::CommandSecondModel) {
                command_second_model(&mut runtime).await;
            }
            let turn = runtime
                .stream_turn(
                    text_input(&root, text),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

fn recorded_models(models: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    models
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// An input sent after a config command runs under the new config (D3 §3.1):
/// `command(M2)` then `send(x)` runs x on M2, and the root before the command
/// ran on M1.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_input_sent_after_a_config_command_runs_on_the_new_model(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "after-command",
        &effect_host,
        &stores,
        Arc::new(crate::SingleProviderResolver::new(recording_model(
            &calls, &models,
        ))),
    )
    .await;
    let first = TurnId::from(format!("{prefix}-turn-config-after-command-first"));
    let turn = run_text_turn(&runner, &parts, &first, "first", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the first root runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the first root finishes: {:?}",
        turn.outcome
    );
    let second = TurnId::from(format!("{prefix}-turn-config-after-command-second"));
    let turn = run_text_turn(
        &runner,
        &parts,
        &second,
        "second",
        BeforeSend::CommandSecondModel,
    )
    .await
    .unwrap_or_else(|error| panic!("the root sent after the command runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the root sent after the command finishes: {:?}",
        turn.outcome
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
        "the root before the command ran on the first model, the one after it on the second"
    );
    let head = parts
        .store
        .load_session_head_meta()
        .await
        .expect("read the head")
        .expect("the session committed");
    assert_eq!(head.config.model.id, SECOND_MODEL);
}

/// The tool whose call closes the first frame with a switch, so the root
/// runs a second physical turn.
const SWITCH_TOOL: &str = "turn_config_switch_probe";

struct SwitchTool;

fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "A tool whose call switches the turn to a follow-on agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for SwitchTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == SWITCH_TOOL).then(|| Arc::new(switch_tool().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("turn-config-switch")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("turn-config follow-on".to_string()),
                },
            ),
        ))
    }
}

/// One config resolution per root (D3 §2.1, Q11): a root whose first frame
/// switches runs two physical turns under one recorded config. Both model
/// calls name the same provider and model, and a tier that can read its
/// journal holds exactly one `turn-config:{root}` entry for the root.
pub async fn one_config_resolution_per_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = {
        let calls = Arc::clone(&calls);
        let models = Arc::clone(&models);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request.model.clone());
                async move {
                    let part = if index == 0 {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "turn-config-switch-call".into(),
                            tool_name: SWITCH_TOOL.into(),
                            input_json: "{}".into(),
                            replay: None,
                        }
                    } else {
                        crate::LlmOutputPart::Text {
                            text: "answered in the follow-on frame".into(),
                            response_meta: None,
                        }
                    };
                    Ok(crate::LlmResponse {
                        parts: vec![part],
                        ..crate::LlmResponse::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let mut parts = law_session(
        prefix,
        "one-resolution",
        &effect_host,
        &stores,
        Arc::new(crate::SingleProviderResolver::new(model)),
    )
    .await;
    parts.tools = vec![Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-turn-config-switch-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool)),
    ))];
    let root = TurnId::from(format!("{prefix}-turn-config-one-resolution-root"));
    let run = run_text_turn(
        &runner,
        &parts,
        &root,
        "switch, then answer",
        BeforeSend::Nothing,
    )
    .await
    .unwrap_or_else(|error| panic!("the switching root runs: {error:?}"));
    assert!(
        matches!(run.outcome, crate::TurnOutcome::Finished(_)),
        "the root finishes in its follow-on frame: {:?}; errors: {:?}",
        run.outcome,
        run.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "precondition: the root ran two physical turns, one model call each"
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_MODEL.to_string(), FIRST_MODEL.to_string()],
        "every physical turn of the root names the one recorded model"
    );
    if let Some(keys) = runner
        .recorded_replay_keys(&crate::ExecutionScope::turn(&parts.session_id, &root))
        .await
    {
        let resolutions = keys
            .iter()
            .filter(|key| key.starts_with("turn-config:"))
            .collect::<Vec<_>>();
        assert_eq!(
            resolutions,
            vec![&format!("turn-config:{root}")],
            "the root resolved its config exactly once: {keys:?}"
        );
    }
}

/// A route this worker cannot bind retries and never fails the turn (D3
/// Q3): the root aborts retryably with nothing recorded as its outcome, and
/// once the provider is back its redrive completes it once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unbindable_route_retries_and_never_fails_the_turn(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let served = law_session(
        prefix,
        "unbindable",
        &effect_host,
        &stores,
        Arc::new(crate::SingleProviderResolver::new(recording_model(
            &calls, &models,
        ))),
    )
    .await;
    // The same session on a worker whose resolver lacks the recorded
    // provider.
    let mut unserved = served.clone();
    unserved.host.providers.provider_resolver = Arc::new(crate::ProviderRegistry::new());
    let root = TurnId::from(format!("{prefix}-turn-config-unbindable-root"));
    let aborted = run_text_turn(&runner, &unserved, &root, "hello", BeforeSend::Nothing)
        .await
        .expect_err("a root whose recorded route cannot be bound aborts");
    assert_eq!(
        aborted.code,
        crate::RuntimeErrorCode::ProviderBindingUnavailable,
        "the abort names the unbindable route: {aborted:?}"
    );
    assert!(
        aborted.is_retryable(),
        "an unbindable route is retried, never the turn's outcome: {aborted:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
    assert!(
        !served
            .store
            .committed_turn_exists(&root)
            .await
            .expect("read the root's commit"),
        "the aborted root recorded no outcome"
    );
    assert!(
        served
            .store
            .load_turn_park(&served.session_id)
            .await
            .expect("read the session's park")
            .is_none(),
        "a retry is not a park"
    );

    let turn = run_text_turn(&runner, &served, &root, "hello", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the redrive with the provider back runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redrive completes the root: {:?}",
        turn.outcome
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the root ran once");
    assert!(
        served
            .store
            .committed_turn_exists(&root)
            .await
            .expect("read the root's commit"),
        "the redrive committed the root"
    );
}

/// A config command whose route no provider of this host serves is refused
/// typed at send, and nothing is enqueued (D3 §3.1).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_bad_route_is_refused_at_send_with_nothing_enqueued(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "bad-route",
        &effect_host,
        &stores,
        Arc::new(crate::SingleProviderResolver::new(recording_model(
            &calls, &models,
        ))),
    )
    .await;
    let scope = TurnId::from(format!("{prefix}-turn-config-bad-route-send"));
    let (refusal_tx, mut refusal_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &scope)),
            {
                let parts = parts.clone();
                Arc::new(move |_scope| {
                    let parts = parts.clone();
                    let refusal_tx = refusal_tx.clone();
                    Box::pin(async move {
                        let mut runtime = build_runtime(parts).await;
                        let sent = runtime
                            .submit_session_command(
                                crate::SessionCommand::ApplyConfigPatch {
                                    patch: Box::new(crate::ApplyConfigPatch {
                                        provider_id: Some(
                                            "turn-config-unknown-provider".to_string(),
                                        ),
                                        ..crate::ApplyConfigPatch::default()
                                    }),
                                },
                                "turn-config-bad-route",
                            )
                            .await;
                        let _ = refusal_tx.send(sent);
                        crate::ConformanceTurnEnd::Settled
                    })
                })
            },
        )
        .await;
    let refusal = refusal_rx
        .recv()
        .await
        .expect("the tier's runner ran the send")
        .expect_err("a route no provider serves is refused at send");
    assert_eq!(
        refusal.code,
        crate::RuntimeErrorCode::ProviderRouteUnknown,
        "the refusal is typed: {refusal:?}"
    );
    assert!(
        parts
            .store
            .list_queued_work(&parts.session_id)
            .await
            .expect("read the session's queued work")
            .is_empty(),
        "nothing was enqueued"
    );
}

/// A route valid when its command was sent but unserved when the command is
/// applied is refused at apply (D3 §3.3): the command settles and changes
/// nothing, and the session keeps its provider. (The typed `Refused`
/// settlement and its refused window are the ingress drain's, FIG-3541/S8.)
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_route_refused_at_apply_leaves_the_route_unchanged(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let alternate = crate::testing::TestProvider::builder()
        .kind("turn-config-alternate")
        .complete_error("the alternate provider is never asked")
        .build()
        .into_handle();
    let session = recording_model(&calls, &models);
    // The worker that sends the command serves the alternate route.
    let sender = law_session(
        prefix,
        "refused-at-apply",
        &effect_host,
        &stores,
        Arc::new(
            crate::ProviderRegistry::new()
                .with(session.clone())
                .and_then(|registry| registry.with(alternate))
                .expect("two distinct providers"),
        ),
    )
    .await;
    // The worker that applies it no longer does.
    let mut applier = sender.clone();
    applier.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(session));
    let store = Arc::clone(&sender.store);
    let session_id = sender.session_id.clone();
    let scope = TurnId::from(format!("{prefix}-turn-config-refused-at-apply"));
    let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&session_id, &scope)),
            Arc::new(move |_scope| {
                let sender = sender.clone();
                let applier = applier.clone();
                let settled_tx = settled_tx.clone();
                Box::pin(async move {
                    let mut sending = build_runtime(sender).await;
                    let receipt = sending
                        .submit_session_command(
                            crate::SessionCommand::ApplyConfigPatch {
                                patch: Box::new(crate::ApplyConfigPatch {
                                    provider_id: Some("turn-config-alternate".to_string()),
                                    ..crate::ApplyConfigPatch::default()
                                }),
                            },
                            "turn-config-refused-at-apply",
                        )
                        .await
                        .expect("a served route is accepted at send");
                    let mut applying = build_runtime(applier).await;
                    let settled = applying.settle_session_command(receipt).await;
                    let _ = settled_tx.send(settled);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    let settled = settled_rx
        .recv()
        .await
        .expect("the tier's runner ran the send and the apply")
        .expect("the command settles");
    assert!(
        matches!(settled, crate::SessionCommandSettlement::Durable(_)),
        "the refused command settles and is not retried: {settled:?}"
    );
    let head = store
        .load_session_head_meta()
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.provider_id, "stub",
        "a route refused at apply leaves the session on its provider"
    );
}
