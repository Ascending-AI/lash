//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A run resolves its session config once, as a recorded step at the top of
//! the logical-turn funnel, and every physical turn of the run executes under
//! that record. A redrive replays the record instead of reading the live
//! head, so a config change that landed after the run committed never
//! reaches the run's replay, and an input that arrives after the change
//! runs under it.

use crate::ActorContext;
use lash_core::testing::TestTurnExecution as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// The model the session starts on.
const FIRST_PROFILE: &str = "mock-model";
/// The model a config command moves the session to.
const SECOND_PROFILE: &str = "turn-config-second-model";

/// Everything a runtime for these laws is built from, shared by every
/// attempt so each is the same session on the same store.
#[derive(Clone)]
struct ConfigParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    /// The protocol the session runs: the standard fake unless the law
    /// needs another.
    protocol: Vec<Arc<dyn crate::plugin::PluginFactory>>,
    /// Plugins the law adds to the protocol.
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
}

async fn build_runtime(parts: ConfigParts) -> crate::LashRuntime {
    build_runtime_under(parts, crate::testing::mock_session_policy()).await
}

/// The law's runtime, opened with `policy` as its creation defaults: what a
/// session with no head yet starts from, and what its first commit records.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime_under(
    parts: ConfigParts,
    policy: crate::SessionPolicy,
) -> crate::LashRuntime {
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(parts.protocol.into_iter().chain(parts.tools).collect())
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .build(),
    )
    .await
    .expect("build the turn-config conformance runtime")
}

/// The host's models for these laws: [`FIRST_PROFILE`] and [`SECOND_PROFILE`],
/// both served by `provider`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: two distinct literal keys always register"
)]
fn turn_config_llm_profiles(provider: crate::ProviderHandle) -> Arc<crate::LlmProfileRegistry> {
    Arc::new(
        crate::LlmProfileRegistry::new()
            .register(
                FIRST_PROFILE,
                crate::RegisteredLlmProfile::new(
                    crate::testing::test_llm_profile_metadata(FIRST_PROFILE),
                    provider.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_PROFILE,
                    crate::RegisteredLlmProfile::new(
                        crate::testing::test_llm_profile_metadata(SECOND_PROFILE),
                        provider,
                    ),
                )
            })
            .expect("two distinct keys register"),
    )
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
                .push(request.model.wire_model().to_string());
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

/// The parts of a turn-config law's session: a host whose models serve the
/// recording model, and the session's store.
async fn law_session(
    prefix: &str,
    name: &str,
    effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
) -> ConfigParts {
    law_session_created_with(prefix, name, effect_host, stores, models, Vec::new()).await
}

/// [`law_session`], except the session is created with `tools` installed:
/// the created head records the plugin configuration a creator on that
/// plugin set resolves — the protocol pointer and every installed owner's
/// namespace (FIG-4379) — so a runtime that opens it later reads exactly the
/// namespaces creation recorded (FIG-4764).
async fn law_session_created_with(
    prefix: &str,
    name: &str,
    effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    law_session_recording(
        prefix,
        name,
        effect_host,
        stores,
        models,
        crate::testing::mock_session_policy(),
        tools,
    )
    .await
}

/// [`law_session`], except the created head records `policy` and the session
/// is created with `tools` installed: the session is created under the
/// policy and plugin set a creating deployment would mint, so a runtime that
/// opens it later adopts exactly the config the law means to record
/// (FIG-4553, FIG-4764).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's own plugin set's creation config resolves"
)]
async fn law_session_recording(
    prefix: &str,
    name: &str,
    _effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    policy: crate::SessionPolicy,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    let session_id = SessionId::fixture(format!("{prefix}-turn-config-{name}-session"));
    let mut host = crate::LawBackend::over_stores(Arc::clone(stores)).host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.models = models;
    // The created head records what a creator on the session's plugin set
    // resolves (FIG-4379). These laws run the standard fake protocol, which
    // owns no plugin configuration; the law's tools are installed at
    // creation because an owner installed only afterwards never reaches the
    // recorded head.
    let protocol = crate::testing::test_standard_protocol_factories();
    let mut config = crate::PersistedSessionConfig::from(&policy);
    config.plugin_config = crate::plugin::PluginHost::new(
        protocol
            .iter()
            .cloned()
            .chain(tools.iter().cloned())
            .collect(),
    )
    .resolve_creation_plugin_config(
        Some("test_protocol"),
        &crate::PluginOptions::default(),
        None,
        true,
        &crate::store::plugin_writers::PluginAdmission::default(),
    )
    .expect("the law's plugin set resolves its creation plugin config");
    let store =
        crate::conformance::law_session_store_with_config(stores.as_ref(), &session_id, config)
            .await;
    ConfigParts {
        session_id,
        host,
        store,
        protocol,
        tools,
    }
}

/// Run `run` with `text` as one attempt on the tier's runner and hand back
/// how its turn returned.
async fn run_text_turn(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    run: &TurnId,
    text: &'static str,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, run)),
            text_attempt(parts, run, text, turn_tx),
        )
        .await;
    turn_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran run `{run}`"))
}

fn text_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    text: &'static str,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            let turn = runtime
                .execute_turn(
                    text_input(&run, text),
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

/// The tool whose call closes the first frame with a switch, so the run
/// runs a second physical turn.
const SWITCH_TOOL: &str = "turn_config_switch_probe";

struct SwitchTool;

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "A tool whose call switches the turn to a follow-on agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
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

/// One config resolution per run (D3 §2.1, Q11): a run whose first frame
/// switches runs two physical turns under one recorded config. Both model
/// calls name the same provider and model, and a tier that can read its
/// journal holds exactly one `turn-config:{run}` entry for the run.
pub async fn one_config_resolution_per_run(
    prefix: &str,
    effect_host: ActorContext,
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
                    .push(request.model.wire_model().to_string());
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
    let parts = law_session_created_with(
        prefix,
        "one-resolution",
        &effect_host,
        &stores,
        turn_config_llm_profiles(model),
        vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-turn-config-switch-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool)),
        ))],
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-turn-config-one-resolution-run"));
    let executed = run_text_turn(&runner, &parts, &run, "switch, then answer")
        .await
        .unwrap_or_else(|error| panic!("the switching run executes: {error:?}"));
    assert!(
        matches!(executed.outcome, crate::TurnOutcome::Finished(_)),
        "the run finishes in its follow-on frame: {:?}; errors: {:?}",
        executed.outcome,
        executed.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "precondition: the run ran two physical turns, one model call each"
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_PROFILE.to_string(), FIRST_PROFILE.to_string()],
        "every physical turn of the run names the one recorded model"
    );
    if let Some(keys) = runner
        .recorded_replay_keys(&crate::ExecutionScope::turn(&parts.session_id, &run))
        .await
    {
        let resolutions = keys
            .iter()
            .filter(|key| key.starts_with("turn-config:"))
            .collect::<Vec<_>>();
        assert_eq!(
            resolutions,
            vec![&format!("turn-config:{run}")],
            "the run resolved its config exactly once: {keys:?}"
        );
    }
}

/// A recorded model this worker cannot bind retries and never fails the turn
/// (D3 Q3): the run aborts retryably with nothing recorded as its outcome,
/// and once the key is served again its redrive completes it once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unbindable_llm_profile_retries_and_never_fails_the_turn(
    prefix: &str,
    effect_host: ActorContext,
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
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    // The same session on a worker whose models lack the recorded key.
    let mut unserved = served.clone();
    unserved.host.providers.models = Arc::new(crate::LlmProfileRegistry::new());
    let run = TurnId::fixture(format!("{prefix}-turn-config-unbindable-run"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    // The first attempt runs on the worker without the key; every later one,
    // the tier's own retry of the unsealed model call included, on the one
    // that serves it.
    let attempt: crate::ConformanceTurnAttempt = {
        let served = text_attempt(&served, &run, "hello", turn_tx.clone());
        let unserved = text_attempt(&unserved, &run, "hello", turn_tx);
        let attempts = Arc::clone(&attempts);
        Arc::new(move |scope| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                unserved(scope)
            } else {
                served(scope)
            }
        })
    };
    let scope = admit(crate::ExecutionScope::turn(&served.session_id, &run));
    runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
    // A tier that returns the unserved attempt's abort hands it back here; an
    // engine that retries the step itself runs the served attempt before
    // this run returns.
    let mut turns = Vec::new();
    while let Ok(turn) = turn_rx.try_recv() {
        turns.push(turn);
    }
    if turns.iter().all(Result::is_err) {
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
        assert!(
            !served
                .store
                .committed_turn_exists(&served.session_id, &run)
                .await
                .expect("read the run's commit"),
            "the aborted run recorded no outcome"
        );
        runner.run_turn(scope, attempt).await;
        while let Ok(turn) = turn_rx.try_recv() {
            turns.push(turn);
        }
    }
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "the unserved attempt did not end the run: {turns:?}"
    );
    for aborted in turns.iter().filter_map(|turn| turn.as_ref().err()) {
        assert_eq!(
            aborted.code,
            crate::RuntimeErrorCode::LlmProfileUnavailable,
            "the abort names the unbindable model: {aborted:?}"
        );
        assert_eq!(
            aborted.profile_key(),
            Some(&crate::LlmProfileKey::new(FIRST_PROFILE)),
            "the abort carries the recorded key typed: {aborted:?}"
        );
        assert!(
            aborted.is_retryable(),
            "an unbindable model is retried, never the turn's outcome: {aborted:?}"
        );
    }
    let finished = turns
        .iter()
        .filter_map(|turn| turn.as_ref().ok())
        .collect::<Vec<_>>();
    assert_eq!(
        finished.len(),
        1,
        "the attempt with the model back completes the run once: {turns:?}"
    );
    assert!(
        matches!(finished[0].outcome, crate::TurnOutcome::Finished(_)),
        "the redrive completes the run: {:?}",
        finished[0].outcome
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the run ran once");
    assert!(
        served
            .store
            .committed_turn_exists(&served.session_id, &run)
            .await
            .expect("read the run's commit"),
        "the redrive committed the run"
    );
}

mod recorded_request_defaults;
pub use recorded_request_defaults::*;

/// The tool a looping model calls on every iteration of its turn.
const LOOKUP_TOOL: &str = "turn_config_lookup_probe";

struct LookupTool;

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn lookup_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{LOOKUP_TOOL}"),
        LOOKUP_TOOL,
        "A tool that answers every call.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
}

#[async_trait::async_trait]
impl crate::ToolProvider for LookupTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![lookup_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == LOOKUP_TOOL).then(|| Arc::new(lookup_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"found": true})),
        ))
    }
}

/// A model that calls [`LOOKUP_TOOL`] on every call and never answers, so a
/// turn runs until its budget stops it. `calls` counts its calls.
fn looping_model(calls: &Arc<AtomicUsize>) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::ToolCall {
                        call_id: format!("turn-config-lookup-{index}"),
                        tool_name: LOOKUP_TOOL.into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A looping-model law's session: [`LookupTool`] installed at creation and
/// `calls` counting the model's calls, the created head recording `recorded`.
async fn looping_session(
    prefix: &str,
    name: &str,
    effect_host: &ActorContext,
    stores: &Arc<dyn crate::StoreSet>,
    calls: &Arc<AtomicUsize>,
    recorded: crate::SessionPolicy,
) -> ConfigParts {
    law_session_recording(
        prefix,
        name,
        effect_host,
        stores,
        turn_config_llm_profiles(looping_model(calls)),
        recorded,
        vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-turn-config-lookup-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(LookupTool)),
        ))],
    )
    .await
}

/// A policy whose execution controls are `turn_budget`, over the mock route.
fn policy_with_budget(turn_budget: crate::TurnBudget) -> crate::SessionPolicy {
    crate::SessionPolicy {
        turn_budget,
        ..crate::testing::mock_session_policy()
    }
}

/// One turn attempt of `run` on a runtime opened with `policy`, sending how
/// the turn returned on `turn_tx`.
fn looping_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    policy: crate::SessionPolicy,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let policy = policy.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime_under(parts, policy).await;
            let turn = runtime
                .execute_turn(
                    text_input(&run, "look everything up"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

/// Crashes a run's execution after its config is recorded and before its
/// first model call.
struct CrashBeforeFirstModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeFirstModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the run's config record and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A redrive runs under the execution controls its run recorded (FIG-4376,
/// ADR 0105 §1). The run's first execution records its config, turn budget
/// included, and dies before its first model call. The redrive opens the
/// session under other creation defaults, as a redeployed worker with another
/// default budget would: it reads the record back and stops at the recorded
/// bound.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_runs_under_the_execution_controls_its_run_recorded(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const RECORDED_TURNS: usize = 2;
    const REDEPLOYED_TURNS: usize = 5;
    let calls = Arc::new(AtomicUsize::new(0));
    // The session is created under the crashing execution's bound: the
    // created head records it, and the redrive's open adopts it (FIG-4553).
    let parts = looping_session(
        prefix,
        "recorded-controls-redrive",
        &effect_host,
        &stores,
        &calls,
        policy_with_budget(crate::TurnBudget::bounded(RECORDED_TURNS)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-turn-config-recorded-controls-run"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let run = run.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let run = run.clone();
            Box::pin(async move {
                let mut runtime = build_runtime_under(
                    parts,
                    policy_with_budget(crate::TurnBudget::bounded(RECORDED_TURNS)),
                )
                .await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .execute_turn(
                        text_input(&run, "look everything up"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash fires before the run's first model call");
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            crashing,
            looping_attempt(
                &parts,
                &run,
                policy_with_budget(crate::TurnBudget::bounded(REDEPLOYED_TURNS)),
                turn_tx,
            ),
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner redrove the run")
        .unwrap_or_else(|error| panic!("the redriven run executes: {error:?}"));
    assert_eq!(
        turn.outcome,
        crate::TurnOutcome::Stopped(crate::TurnStop::MaxTurns),
        "the redriven run stops at a bound"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        RECORDED_TURNS,
        "the redriven run stops at the bound its run recorded, not the redrive's default"
    );
}

/// A worker's termination policy that says a turn ending without `Done`
/// fails when `missing_done_fails`, and finishes otherwise.
fn termination(missing_done_fails: bool) -> crate::TerminationPolicy {
    crate::TerminationPolicy {
        treat_missing_done_as_failure: missing_done_fails,
    }
}

/// A missing run record refuses terminal assembly without panicking,
/// retrying, or losing its code at a plugin or host boundary (FIG-4508).
#[expect(clippy::expect_used, reason = "conformance fixture results must exist")]
pub async fn a_missing_recorded_termination_is_a_typed_terminal_refusal(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "missing-recorded-termination",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-missing-recorded-termination-run"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            Arc::new(move |scope| {
                let parts = parts.clone();
                let run = run.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let result = runtime
                        .finish_without_recorded_run_for_testing(
                            run,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await;
                    let _ = tx.send(result);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    let error = rx
        .recv()
        .await
        .expect("the commit attempt returned")
        .expect_err("a run without its record cannot assemble a terminal");
    let expected = crate::RuntimeErrorCode::from_wire_code("recorded_termination_unavailable");
    assert_eq!(error.code, expected);
    assert!(!error.is_retryable());
    assert!(error.is_terminal());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let plugin = crate::plugin::PluginError::Runtime(error.clone());
    let encoded = serde_json::to_vec(&plugin).expect("encode the plugin refusal");
    let decoded: crate::plugin::PluginError =
        serde_json::from_slice(&encoded).expect("decode the plugin refusal");
    let returned = decoded.into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the plugin boundary retains the cause"
    );
    assert!(!returned.is_retryable());
    let host = crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned));
    let crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned)) = host else {
        panic!("the host retains the typed runtime refusal");
    };
    assert_eq!(returned.code, expected);
    let controller = crate::RuntimeEffectControllerError::from(error);
    let encoded = serde_json::to_vec(&controller).expect("encode the controller refusal");
    let decoded: crate::RuntimeEffectControllerError =
        serde_json::from_slice(&encoded).expect("decode the controller refusal");
    let returned = crate::plugin::PluginError::RuntimeEffectController(decoded)
        .into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the controller boundary retains the cause"
    );
    assert!(!returned.is_retryable());
}

/// One attempt of `run` on a worker whose host termination policy is
/// `termination`, under the protocol that ends its turn without `Done`.
/// With `crash`, the attempt dies after the run's config record and before
/// its first model call; otherwise it sends how the turn returned on
/// `turn_tx`.
fn missing_done_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    termination: crate::TerminationPolicy,
    crash: bool,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let mut parts = parts.clone();
    parts.host.control.termination = termination;
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
            }
            let turn = runtime
                .execute_turn(
                    text_input(&run, "answer without ending the stream"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            assert!(!crash, "the crash fires before the run's first model call");
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

/// Whether `turn` carries the missing-`Done` fallback's issue.
fn has_missing_done_issue(turn: &crate::AssembledTurn) -> bool {
    turn.errors
        .iter()
        .any(|issue| issue.code == Some(crate::TurnFailureCode::MissingDone.into()))
}

/// A redrive assembles the terminal its run's recorded termination policy
/// decides (FIG-4389, ADR 0105 §1). The turn's protocol ends its stream with
/// neither an outcome nor `Done`, so its terminal is the missing-`Done`
/// fallback. The run's first execution, on a worker with one policy,
/// records its config and dies before its first model call; the redrive runs
/// on a worker with the opposite policy. Both directions assemble the
/// terminal the recorded policy decides: a runtime error with a `MissingDone`
/// issue when it fails a missing `Done`, a finished turn without that issue
/// when it does not.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_assembles_the_terminal_its_run_recorded_termination_decides(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for recorded_fails in [true, false] {
        let name = if recorded_fails {
            "missing-done-recorded-fails"
        } else {
            "missing-done-recorded-finishes"
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let models = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut parts = law_session(
            prefix,
            name,
            &effect_host,
            &stores,
            turn_config_llm_profiles(recording_model(&calls, &models)),
        )
        .await;
        parts.protocol = crate::testing::test_protocol_factories_ending_without_done();
        let run = TurnId::fixture(format!("{prefix}-turn-config-{name}-run"));
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_crashed_then_redriven_turn(
                admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
                missing_done_attempt(
                    &parts,
                    &run,
                    termination(recorded_fails),
                    true,
                    turn_tx.clone(),
                ),
                missing_done_attempt(&parts, &run, termination(!recorded_fails), false, turn_tx),
            )
            .await;
        let turn = turn_rx
            .recv()
            .await
            .expect("the tier's runner redrove the run")
            .unwrap_or_else(|error| panic!("{name}: the redriven run executes: {error:?}"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the redriven run makes its one model call and its stream ends there"
        );
        if recorded_fails {
            assert_eq!(
                turn.outcome,
                crate::TurnOutcome::Stopped(crate::TurnStop::RuntimeError),
                "{name}: the redrive fails the missing Done, as its run recorded"
            );
            assert!(
                has_missing_done_issue(&turn),
                "{name}: the failure is the missing Done: {:?}",
                turn.errors
            );
        } else {
            assert!(
                matches!(
                    turn.outcome,
                    crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { .. })
                ),
                "{name}: the redrive finishes the turn, as its run recorded: {:?}",
                turn.outcome
            );
            assert!(
                !has_missing_done_issue(&turn),
                "{name}: the run recorded no missing-Done failure: {:?}",
                turn.errors
            );
        }
    }
}
