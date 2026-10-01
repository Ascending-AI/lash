// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
#[cfg(feature = "rlm")]
use crate::rlm::RlmSendBuilderExt as _;
#[path = "session_lifecycle/commit_budget.rs"]
mod commit_budget;

#[path = "session_lifecycle/journal_retirement.rs"]
mod journal_retirement;
#[path = "session_lifecycle/session_binding.rs"]
mod session_binding;

fn persisted_tool_state_at_generation(
    state: lash_core::ToolState,
    generation: u64,
) -> lash_core::ToolState {
    let mut value = serde_json::to_value(state).expect("serialize persisted tool state");
    value["generation"] = serde_json::json!(generation);
    serde_json::from_value(value).expect("deserialize persisted tool state")
}

#[cfg(feature = "rlm")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReconciliationTransformObservation {
    max_context_tokens: Option<usize>,
    session_model: String,
}

#[cfg(feature = "rlm")]
struct ReconciliationTransformProbe {
    observations: Arc<std::sync::Mutex<Vec<ReconciliationTransformObservation>>>,
}

#[cfg(feature = "rlm")]
struct ReconciliationProbeFactory {
    transform: Arc<dyn lash_core::facade_support::TurnContextTransform>,
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::PluginFactory for ReconciliationProbeFactory {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(ReconciliationProbePlugin {
            transform: Arc::clone(&self.transform),
        }))
    }
}

#[cfg(feature = "rlm")]
struct ReconciliationProbePlugin {
    transform: Arc<dyn lash_core::facade_support::TurnContextTransform>,
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::SessionPlugin for ReconciliationProbePlugin {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.context().prepare_turn(0, Arc::clone(&self.transform));
        Ok(())
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::facade_support::TurnContextTransform for ReconciliationTransformProbe {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    async fn transform(
        &self,
        ctx: &lash_core::facade_support::TurnTransformContext<'_>,
        input: lash_core::facade_support::PreparedContext,
    ) -> std::result::Result<
        lash_core::facade_support::PreparedContext,
        lash_core::facade_support::ContextError,
    > {
        self.observations
            .lock_recover()
            .push(ReconciliationTransformObservation {
                max_context_tokens: ctx.max_context_tokens,
                session_model: ctx
                    .state
                    .policy()
                    .wire_model()
                    .unwrap_or_default()
                    .to_string(),
            });
        Ok(input)
    }
}

fn conflicting_reopen_state(session_id: &SessionId) -> RuntimeSessionState {
    let historical_policy = lash_core::SessionPolicy {
        model: Some(recorded_model(model_spec("historical-model", None, 11_111))),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let current_policy = lash_core::SessionPolicy {
        model: Some(recorded_model(model_spec(
            "current-frame-model",
            None,
            22_222,
        ))),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        policy: historical_policy.clone(),
        agent_frames: Vec::new(),
        current_frame_node_id: None,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let frame_key =
        lash_core::FrameKey::from_caller_material(&format!("conflicting-frame-{session_id}"))
            .expect("non-empty frame material");
    let frame_node_id = lash_core::facade_support::frame_node_id(session_id, frame_key.as_str());
    let mut nodes = state.session_graph.nodes.clone();
    nodes.push(std::sync::Arc::new(lash_core::SessionNodeRecord {
        node_id: frame_node_id.to_string().into(),
        parent_node_id: state.session_graph.leaf_node_id.clone(),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core::AgentFrameReason::continue_as(),
            assignment: lash_core::AgentFrameAssignment::unconfigured(current_policy),
        },
    }));
    state.session_graph =
        lash_core::SessionGraph::from_shared_nodes(nodes, Some(frame_node_id.to_string().into()))
            .expect("session lifecycle fixture graph is valid");
    state.current_frame_node_id = Some(frame_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(session_id);
    state.policy = lash_core::SessionPolicy {
        model: Some(recorded_model(model_spec("top-level-model", None, 33_333))),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    state
}

#[cfg(feature = "rlm")]
#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CompileSurfaceToolConfig {
    tool_name: String,
}

#[cfg(feature = "rlm")]
struct CompileSurfaceToolFactory {
    id: &'static str,
    default_tool_name: &'static str,
}

#[cfg(feature = "rlm")]
impl CompileSurfaceToolFactory {
    fn new(id: &'static str, default_tool_name: &'static str) -> Self {
        Self {
            id,
            default_tool_name,
        }
    }
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::PluginFactory for CompileSurfaceToolFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        let config = ctx
            .plugin_config
            .decode::<CompileSurfaceToolConfig>(self.id)
            .map_err(|err| lash_core::PluginError::Registration(err.to_string()))?;
        let tool_name = config
            .map(|config| config.tool_name)
            .unwrap_or_else(|| self.default_tool_name.to_string());
        Ok(Arc::new(CompileSurfaceToolPlugin {
            plugin_id: self.id,
            tool_name,
        }))
    }
}

#[cfg(feature = "rlm")]
struct CompileSurfaceToolPlugin {
    plugin_id: &'static str,
    tool_name: String,
}

#[cfg(feature = "rlm")]
impl lash_core::facade_support::SessionPlugin for CompileSurfaceToolPlugin {
    fn id(&self) -> &'static str {
        self.plugin_id
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.tools().provider(Arc::new(CompileSurfaceToolProvider {
            tool_name: self.tool_name.clone(),
        }))?;
        Ok(())
    }
}

#[cfg(feature = "rlm")]
struct CompileSurfaceToolProvider {
    tool_name: String,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::ToolProvider for CompileSurfaceToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![compile_surface_tool_definition(&self.tool_name).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool_name)
            .then(|| Arc::new(compile_surface_tool_definition(&self.tool_name).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
    }
}

#[cfg(feature = "rlm")]
pub(super) fn compile_surface_tool_definition(name: &str) -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name.to_string(),
            "Compile-surface test tool.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        ),
        name.to_string(),
    )
}

#[tokio::test]
async fn standard_core_runs_mock_turn() -> Result<()> {
    let core = standard_core().await;
    let session = core.session("main").created().await.open().await?;
    let events = RecordingEvents::default();

    let result = session
        .send(TurnInput::text("hello"))
        .output_into(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::AssistantProseDelta { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::ToolCallCompleted { .. }))
    );
    Ok(())
}

/// The backend is a required argument, so a build without one cannot be
/// written (the `core_builder_requires_a_backend` UI case). What the
/// builder still refuses at `build()` is a missing runtime setting.
#[tokio::test]
async fn typed_core_builders_require_explicit_runtime_settings() {
    let err = match LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    )
    .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
    {
        Ok(_) => panic!("the standard preset must not default a commit budget"),
        Err(err) => err,
    };
    assert!(matches!(err, EmbedError::MissingCommitBudget));
}

#[tokio::test]
async fn generic_lash_core_builder_requires_protocol_plugin() {
    let err = match explicit_ephemeral_facets(LashCore::builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
    {
        Ok(_) => panic!("generic LashCore must require an explicit protocol plugin"),
        Err(err) => err,
    };

    assert!(matches!(err, EmbedError::MissingProtocolPlugin));
}

/// The standard prompt is recorded config (FIG-4589): the core's default
/// spec states it for a session that states none, a session's own spec
/// replaces it, and a prompt command replaces it for the roots after it.
#[tokio::test]
async fn the_standard_prompt_comes_from_the_core_spec_the_session_spec_and_its_command()
-> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(
        recording_prompt_provider(Arc::clone(&seen)),
        mock_model_spec(),
    )
    .session_plugin(
        crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
        crate::standard::StandardTurnOptions {
            prompt: Some(crate::standard::StandardPrompt {
                intro: Some("Core intro.".to_string()),
                instructions: vec!["Core instruction.".to_string()],
                ..Default::default()
            }),
            render: None,
        },
    )
    .map_err(EmbedError::ProtocolTurnOptions)?
    .build(crate::testing::runtime_lease_owner())?;

    let defaulted = core
        .session("prompt-core-default")
        .created()
        .await
        .open()
        .await?;
    defaulted.send(TurnInput::text("first")).output().await?;

    core.session("prompt-own")
        .create(crate::SessionCreation {
            spec: lash_core::facade_support::SessionSpec::new()
                .plugin(
                    crate::standard::STANDARD_PROTOCOL_PLUGIN_ID,
                    crate::standard::StandardTurnOptions {
                        prompt: Some(crate::standard::StandardPrompt {
                            intro: Some("Session intro.".to_string()),
                            instructions: vec!["Session instruction.".to_string()],
                            ..Default::default()
                        }),
                        render: None,
                    },
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ..Default::default()
        })
        .await?;
    let own = core.session("prompt-own").open().await?;
    own.send(TurnInput::text("second")).output().await?;
    own.admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::standard::SetStandardPrompt {
                prompt: crate::standard::StandardPrompt {
                    instructions: vec!["Commanded instruction.".to_string()],
                    ..Default::default()
                },
            },
        ))
        .await?;
    own.send(TurnInput::text("third")).output().await?;

    let prompts = seen.lock_recover();
    assert_eq!(prompts.len(), 3);
    assert_eq!(
        prompts[0].matches("Core intro.").count(),
        1,
        "the core spec's intro is stated once: {}",
        prompts[0]
    );
    assert!(prompts[0].contains("Core instruction."));
    assert!(prompts[1].contains("Session intro."));
    assert!(prompts[1].contains("Session instruction."));
    assert!(
        !prompts[1].contains("Core"),
        "a session's stated prompt replaces the core spec's: {}",
        prompts[1]
    );
    assert!(prompts[2].contains("Commanded instruction."));
    assert!(
        !prompts[2].contains("Session"),
        "the command replaces the whole prompt: {}",
        prompts[2]
    );
    Ok(())
}

/// A session created with its own key runs that key's transport, and a
/// model change naming a key the host does not serve is refused typed and
/// changes nothing (FIG-4374).
#[tokio::test]
async fn a_session_key_selects_its_transport_and_an_unserved_key_is_refused_typed() -> Result<()> {
    let core_provider = text_provider("core-provider", "core-model", "core");
    let session_provider = text_provider("session-provider", "session-model", "session");
    let models = test_catalog(core_provider, [model_spec("core-model", None, 200_000)]);
    let registry = Arc::try_unwrap(models)
        .unwrap_or_else(|_| unreachable!("a fresh catalog has one owner"))
        .register(
            "session-model",
            lash_core::RegisteredModel::new(
                model_spec("session-model", None, 200_000),
                session_provider,
            ),
        )
        .expect("a second key registers");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .models(Arc::new(registry))
    .model("core-model")
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    core.session("main")
        .create(crate::SessionCreation {
            spec: crate::SessionSpec::inherit().model("session-model"),
            ..Default::default()
        })
        .await?;
    let session = core.session("main").open().await?;

    let session_result = session.send(TurnInput::text("hello")).output().await?;
    assert_eq!(assistant_prose(&session_result.activities), "session");

    // A model change names a key: one the host does not serve is refused
    // typed by the core owner when the transaction resolves, and the session
    // keeps its recorded model.
    let config = session.admin().config();
    let revision = config.revision().await?;
    let refused = config
        .apply(
            crate::config::ConfigWrite::new("unserved-key", revision),
            crate::config::ConfigTransaction::of(crate::config::SetModel {
                model: lash_core::ModelKey::new("updated-model"),
            }),
        )
        .await?;
    let crate::config::ConfigTransactionOutcome::Refused { refusal } = refused else {
        panic!("a key the host does not serve is refused: {refused:?}");
    };
    assert_eq!(refusal.owner, crate::config::CORE_CONFIG_OWNER);
    assert_eq!(
        refusal.owner_refusal::<crate::config::CoreConfigRefusal>(),
        Some(crate::config::CoreConfigRefusal::UnknownModel {
            key: lash_core::ModelKey::new("updated-model"),
        }),
        "the refusal is the core owner's typed unknown-model refusal: {refusal:?}"
    );

    let after_refusal = session.send(TurnInput::text("hello")).output().await?;
    assert_eq!(assistant_prose(&after_refusal.activities), "session");
    Ok(())
}

/// The core's default reasoning is recorded with the session's model and
/// reaches the request unchanged.
#[tokio::test]
async fn the_core_reasoning_is_recorded_and_reaches_the_request() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(
        recording_text_provider(
            "core-provider",
            "core-model",
            Some("core-variant"),
            "core",
            Arc::clone(&seen),
        ),
        model_spec("core-model", Some("core-variant".to_string()), 200_000),
    )
    .reasoning(lash_core::ReasoningSelection::Effort(
        "core-variant".to_string(),
    ))
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core");
    let session = core.session("main").created().await.open().await?;
    assert_eq!(
        session.policy_snapshot().model.map(|model| model.reasoning),
        Some(lash_core::ReasoningSelection::Effort(
            "core-variant".to_string()
        ))
    );

    session.send(TurnInput::text("hello")).output().await?;
    assert_eq!(
        *seen.lock_recover(),
        vec![(
            "core-model".to_string(),
            lash_core::ReasoningSelection::Effort("core-variant".to_string()),
        )]
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_core_opens_rlm_session() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    core.session("rlm").created().await.open().await?;
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_protocol_config_sleep_ability_drives_prompt_surface() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-abilities-prompt-test")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock_recover().push(system_text(&request));
                    Ok(text_response(&typescript_block("finish(\"ok\");")))
                }
            }
        })
        .build()
        .into_handle();
    let config: crate::rlm::RlmProtocolPluginConfig = serde_json::from_value(serde_json::json!({
        "channel": "cell",
        "instruction_limit": { "bounded": 1_000_000 },
        "memory_limit": { "bounded": 67_108_864 },
        "lashlang_abilities": { "sleep": true }
    }))
    .expect("rlm config");
    let backend = double_backend().await;
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        config,
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend.clone(),
    );
    let core = LashCore::rlm_builder(
        backend,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        factory,
    )
    .serve_test_model(provider, mock_model_spec())
    .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-abilities-prompt")
        .created()
        .await
        .open()
        .await?;

    session
        .send(TurnInput::text("hello"))
        .require_finish()?
        .output()
        .await?;

    let prompts = seen.lock_recover();
    // `sleep` is the one surviving ability (FIG-2999): processes, signals and
    // triggers are catalogue presence now, not configuration, so this session —
    // whose catalogue carries neither — is told about `sleep` and about nothing
    // it cannot call. The retired special forms are named nowhere.
    assert!(prompts[0].contains("`await sleep(ms)` pauses the program."));
    for retired in ["registerTrigger", "defineProcess", "triggers.list"] {
        assert!(
            !prompts[0].contains(retired),
            "the prompt still advertises `{retired}`"
        );
    }
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_completed_finish_is_single_copy_in_next_turn_request() -> Result<()> {
    const ANSWER: &str = "FIG-461 terminal answer";

    let trace_path = std::env::temp_dir().join(format!(
        "lash-fig-461-history-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-finish-history-test")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    let request_index = {
                        let mut seen = seen.lock_recover();
                        seen.push(request_text(&request));
                        seen.len()
                    };
                    let answer = match request_index {
                        1 => ANSWER,
                        2 => "second answer",
                        other => panic!("unexpected provider request {other}"),
                    };
                    Ok(text_response(&typescript_block(&format!(
                        "finish({answer:?});"
                    ))))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(provider, mock_model_spec())
        .trace_jsonl_path(trace_path.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-finish-history-single-copy")
        .created()
        .await
        .open()
        .await?;

    session
        .send(TurnInput::text("first"))
        .require_finish()?
        .output()
        .await?;
    Box::pin(session.admin().state().append_messages(vec![
            lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, ANSWER)
                .with_id("workbench-assistant:fig-461-turn-1"),
        ]))
    .await?;
    session
        .send(TurnInput::text("second"))
        .require_finish()?
        .output()
        .await?;
    core.flush_trace_sink()?;

    let seen = seen.lock_recover();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].matches(ANSWER).count(),
        1,
        "the committed terminal answer must occur exactly once in turn 2"
    );
    let trace = std::fs::read_to_string(&trace_path).expect("read FIG-461 trace");
    let llm_starts = lash_trace::parse_jsonl_records::<serde_json::Value>(&trace)
        .expect("trace event JSON")
        .into_iter()
        .filter(|event| event["type"] == "llm_call_started")
        .collect::<Vec<_>>();
    assert_eq!(llm_starts.len(), 2);
    let turn_two_messages = serde_json::to_string(&llm_starts[1]["request"]["messages"])?;
    assert_eq!(
        turn_two_messages.matches(ANSWER).count(),
        1,
        "the sentence must occur exactly once in turn 2 llm_call_started request.messages"
    );
    let _ = std::fs::remove_file(trace_path);
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_multi_turn_finish_history_preserves_observed_lashlang_few_shots() -> Result<()> {
    const ANSWERS: [&str; 3] = [
        "first committed answer",
        "second committed answer",
        "third committed answer",
    ];

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-multi-turn-history-shape-test")
        .complete({
            let calls = Arc::clone(&calls);
            move |request| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let text = request_text(&request);
                    if call > 0 {
                        let observed_turns = call.div_ceil(2);
                        for turn in 1..=observed_turns {
                            assert!(
                                text.contains(&typescript_block(&format!(
                                    "print(\"turn {turn} observation\");"
                                ))),
                                "request {call} lost the paired emission-format cell for turn {turn}"
                            );
                        }
                    }
                    if call == 2 || call == 4 {
                        let completed_turns = call / 2;
                        for (index, answer) in ANSWERS.iter().take(completed_turns).enumerate() {
                            assert_eq!(
                                text.matches(answer).count(),
                                1,
                                "turn {} answer must be single-copy in request {call}",
                                index + 1
                            );
                        }
                    }

                    let turn = call / 2;
                    let response = if call.is_multiple_of(2) {
                        typescript_block(&format!("print(\"turn {} observation\");", turn + 1))
                    } else {
                        typescript_block(&format!("finish({:?});", ANSWERS[turn]))
                    };
                    Ok(text_response(&response))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(provider, mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-multi-turn-history-shape")
        .created()
        .await
        .open()
        .await?;

    for (turn, answer) in ANSWERS.iter().enumerate() {
        session
            .send(TurnInput::text(format!("turn {}", turn + 1)))
            .require_finish()?
            .output()
            .await?;
        Box::pin(session.admin().state().append_messages(vec![
                lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, *answer)
                    .with_id(format!("workbench-assistant:few-shot-turn-{}", turn + 1)),
            ]))
        .await?;
    }

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_compile_surface_uses_core_plugins_extra_plugins_and_request_options() -> Result<()> {
    // The compile APIs are now operations over the RLM factory and a plugin host
    // the caller builds. The plugin host carries the core tool plugin plus any
    // extra tool plugins; the request's execution env plugin options configure
    // them (here `compile-extra-tool` resolves to `lookup`).
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend.clone());
    let factory = Arc::new(rlm_factory(&backend));
    let plugin_host = lash_core::facade_support::PluginHost::new(vec![
        Arc::clone(&factory) as Arc<dyn PluginFactory>,
        Arc::new(CompileSurfaceToolFactory::new(
            "compile-core-tool",
            "compile_core_tool",
        )),
        Arc::new(CompileSurfaceToolFactory::new(
            "compile-extra-tool",
            "fallback",
        )),
    ]);
    // Process lifecycle available for the compile surface (parity with the old
    // core that wired a process registry).
    let process_lifecycle_available = true;
    let plugin_config = || {
        let mut config = lash_core::PluginConfig::default();
        config.insert(
            "compile-extra-tool",
            serde_json::to_value(CompileSurfaceToolConfig {
                tool_name: "lookup".to_string(),
            })
            .expect("compile plugin config serializes"),
        );
        lash_core::AdmittedPluginConfig::new(config, 0)
    };
    let request = crate::rlm::LashlangCompileSurfaceRequest::new(
        "compile-surface",
        lash_core::ProcessExecutionEnvSpec::new(
            plugin_config(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ),
        ),
    );

    let surface =
        factory.lashlang_compile_surface(&plugin_host, process_lifecycle_available, request)?;

    assert!(surface.host_environment.abilities.sleep);
    assert!(surface.tool_catalog.has_callable_tool("compile_core_tool"));
    assert!(surface.tool_catalog.has_callable_tool("lookup"));
    assert!(!surface.tool_catalog.has_callable_tool("fallback"));
    assert!(
        surface
            .host_environment
            .resources
            .resolve_module_operation("Tools", "tools", "compile_core_tool")
            .is_some()
    );
    assert!(
        surface
            .host_environment
            .resources
            .resolve_module_operation("Tools", "tools", "lookup")
            .is_some()
    );

    let compiled = factory
        .compile_lashlang_module(
            &plugin_host,
            process_lifecycle_available,
            crate::rlm::LashlangModuleCompileRequest::new(
                "compile-module",
                r#"
const value = await tools.lookup({});
finish(value);
"#,
                lash_core::ProcessExecutionEnvSpec::new(
                    plugin_config(),
                    lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                    ),
                ),
            ),
        )
        .expect("compile module through the RLM factory");
    artifact_store
        .publish_module_artifact(
            &lash_core::testing::host_pin_claim_for_testing(),
            &compiled.artifact,
        )
        .await
        .expect("publish compiled module");
    assert!(
        artifact_store
            .get_module_artifact(&compiled.module_ref)
            .await
            .expect("load persisted module artifact")
            .is_some(),
        "explicit publication should persist through the configured artifact store"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn rlm_root_session_final_answer_format_defaults_to_markdown_and_can_be_raw() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(
            recording_request_provider(Arc::clone(&seen)),
            mock_model_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;

    let markdown = core
        .session("rlm-root-markdown")
        .created()
        .await
        .open()
        .await?;
    markdown.send(TurnInput::text("hello")).output().await?;

    core.session("rlm-root-raw")
        .create(crate::SessionCreation {
            spec: lash_core::facade_support::SessionSpec::new().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                        ..lash_rlm_types::RlmCreateExtras::default()
                    },
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
            ..Default::default()
        })
        .await?;
    let raw = core.session("rlm-root-raw").open().await?;
    raw.send(TurnInput::text("hello"))
        .require_finish()?
        .output()
        .await?;

    let prompts = seen.lock_recover();
    assert!(prompts[0].contains("=== FINAL ANSWER FORMAT ==="));
    assert!(prompts[0].contains("Markdown string"));
    assert!(!prompts[1].contains("=== FINAL ANSWER FORMAT ==="));
    assert!(!prompts[1].contains("Markdown string"));
    Ok(())
}

/// FIG-1555 clobber 1, end to end: a recorded final-answer format must survive
/// a reopen that says nothing about it.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_recorded_final_answer_format_survives_a_reopen_that_states_nothing() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(
            recording_request_provider(Arc::clone(&seen)),
            mock_model_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;

    core.session("rlm-format-survives-reopen")
        .create(crate::SessionCreation {
            spec: lash_core::facade_support::SessionSpec::new().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                        ..lash_rlm_types::RlmCreateExtras::default()
                    },
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
            ..Default::default()
        })
        .await?;
    let raw = core.session("rlm-format-survives-reopen").open().await?;
    raw.send(TurnInput::text("hello"))
        .require_finish()?
        .output()
        .await?;
    Box::pin(raw.close()).await?;
    let reopened = core
        .session("rlm-format-survives-reopen")
        .created()
        .await
        .open()
        .await?;
    reopened
        .send(TurnInput::text("again"))
        .require_finish()?
        .output()
        .await?;

    let prompts = seen.lock_recover();
    assert_eq!(prompts.len(), 2);
    assert!(
        !prompts[1].contains("=== FINAL ANSWER FORMAT ==="),
        "the reopened session must keep its recorded raw-final-value format"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn malformed_rlm_create_extras_fail_child_session_creation() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let _parent = core.session("rlm-root").created().await.open().await?;
    let mut plugin_options = lash_core::PluginOptions {
        plugins: BTreeMap::new(),
    };
    plugin_options.plugins.insert(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
        serde_json::json!({
            "termination": {
                "kind": "unknown"
            }
        }),
    );

    let err = match core
        .session("rlm-child-bad-extras")
        .create(crate::SessionCreation {
            parent: Some("rlm-root".into()),
            spec: lash_core::facade_support::SessionSpec::new().plugin_options(plugin_options),
        })
        .await
    {
        Ok(_) => panic!("malformed RLM create extras should fail session creation"),
        Err(error) => error,
    };

    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = &err
    else {
        panic!("expected a typed session config refusal, got: {err:?}");
    };
    assert_eq!(refusal.owner, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
    assert_eq!(refusal.at, lash_core::RefusalSite::Creation);
    assert!(
        matches!(
            refusal.reason,
            lash_core::ConfigRefusalReason::Unreadable {
                role: lash_core::ConfigValueRole::CreationInput,
                ..
            }
        ),
        "{refusal:?}"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn cold_open_surfaces_v5_execution_snapshot_rejection_with_operator_remedy() -> Result<()> {
    // Named-field MessagePack for a v5 RLM envelope. The embedded vars payload
    // is deliberately empty: version rejection must happen before Lashlang
    // decode, just as it did for snapshots persisted by the old build.
    let old_version_snapshot = [
        &[0x85][..],
        &[0xa7][..],
        b"version",
        &[0x05][..],
        &[0xa6][..],
        b"engine",
        &[0xa8][..],
        b"lashlang",
        &[0xa4][..],
        b"vars",
        &[0xc4, 0x00][..],
        &[0xa5][..],
        b"files",
        &[0x80][..],
        &[0xb4][..],
        b"deferred_resolutions",
        &[0x81, 0xab][..],
        b"resolutions",
        &[0x80][..],
    ]
    .concat();
    let session_id = "rlm-v5-cold-open";
    let policy = lash_core::SessionPolicy {
        model: Some(recorded_model(mock_model_spec())),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let mut state = RuntimeSessionState {
        session_id: SessionId::from(session_id),
        policy,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.set_execution_state_snapshot(Some(old_version_snapshot.into()));
    let (backend, _) = backend_seeded(state).await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_model(mock_provider(), mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let error = match core.session(session_id).created().await.open().await {
        Ok(_) => panic!("cold open must reject the persisted v5 execution snapshot"),
        Err(error) => error,
    };
    let EmbedError::Session(SessionError::Protocol(message)) = &error else {
        panic!("expected typed protocol rejection at the host boundary, got {error}");
    };

    let expected = format!(
        "RLM snapshot version 5 is incompatible with version {}",
        lash_protocol_rlm::RLM_SNAPSHOT_VERSION
    );
    assert!(message.contains(&expected));
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
    Ok(())
}

#[tokio::test]
async fn store_factory_reopens_persisted_session_state() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("persisted"),
        policy: lash_core::SessionPolicy {
            model: Some(recorded_model(mock_model_spec())),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "already stored",
    )]);
    let (backend, _) = backend_seeded(state).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let reopened = core.session("persisted").created().await.open().await?;
    let messages = reopened.read_view().messages().to_vec();
    assert_eq!(messages.len(), 1);
    assert_eq!(message_text(&messages[0]), "already stored");
    Ok(())
}

#[tokio::test]
async fn cold_reopen_restores_its_committed_generation() -> Result<()> {
    let expected = lash_core::GenerationOptions {
        seed: Some(11),
        ..Default::default()
    };
    let mut persisted_policy = lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
        lash_core::MaxToolCalls::new(1024),
    );
    persisted_policy.model = Some(recorded_model(mock_model_spec()));
    persisted_policy.generation = expected.clone();
    let persisted = RuntimeSessionState {
        session_id: SessionId::from("committed-session"),
        policy: persisted_policy,
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    let (backend, _) = backend_seeded(persisted).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let reopened = core
        .session("committed-session")
        .created()
        .await
        .open()
        .await?;

    assert_eq!(reopened.policy_snapshot().generation, expected);
    Ok(())
}

#[tokio::test]
async fn park_then_resume_preserves_session_transcript() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("parked").created().await.open().await?;
    session.send(TurnInput::text("hello")).output().await?;
    let before = session
        .read_view()
        .messages()
        .iter()
        .map(message_text)
        .collect::<Vec<_>>();
    assert!(
        before.contains(&"hello".to_string()),
        "the pre-park transcript records the turn"
    );

    // Park flushes and drops the live runtime, returning a cheap handle.
    let parked = Box::pin(session.park()).await?;
    assert_eq!(parked.session_id(), "parked");

    // Resume rebuilds a live session; the flushed transcript is visible again.
    let resumed = Box::pin(core.resume(parked)).await?;
    let after = resumed
        .read_view()
        .messages()
        .iter()
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(after, before, "resume must restore the parked transcript");
    // The resumed session is live and can take another turn on top of the
    // restored transcript.
    resumed.send(TurnInput::text("again")).output().await?;
    assert!(
        resumed
            .read_view()
            .messages()
            .iter()
            .map(message_text)
            .any(|text| text == "again")
    );
    Ok(())
}

// FIG-882: `LashRuntime::resume` falls back to a fresh empty state when the
// parked store reports no persisted session, which in isolation looks like it
// would hand a caller a blank conversation under a deleted session's id. The
// binding guard behind that fallback is what makes it safe, so the refusal is
// pinned end to end at the facade: a deleted-while-parked resume must fail with
// the typed tombstone, never succeed with an empty transcript.
#[tokio::test]
async fn resume_of_a_session_deleted_while_parked_refuses_with_a_typed_tombstone() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend_explicit_reconcile().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("deleted-while-parked")
        .created()
        .await
        .open()
        .await?;
    session.send(TurnInput::text("hello")).output().await?;
    let parked = Box::pin(session.park()).await?;

    delete_bound_session(&core, "deleted-while-parked").await?;
    assert!(
        core.session("deleted-while-parked")
            .durable()
            .await?
            .was_deleted()
            .await?,
        "the delete must leave a durable tombstone for the parked id"
    );

    let error = match Box::pin(core.resume(parked)).await {
        Ok(_) => panic!("resume must refuse a session deleted while parked"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            EmbedError::Session(SessionError::Store {
                source: lash_core::StoreError::SessionDeleted { session_id },
                ..
            }) if session_id == "deleted-while-parked"
        ),
        "resume must surface the typed tombstone, got {error}"
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    Ok(())
}

#[tokio::test]
async fn park_with_a_live_handle_reports_session_still_in_use() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("busy").created().await.open().await?;
    // A live clone shares the underlying runtime handle, exactly as an in-flight
    // turn would: parking must refuse rather than silently flush a session that
    // something else is still driving.
    let live_clone = session.clone();
    let err = match Box::pin(session.park()).await {
        Ok(_) => panic!("park must not proceed while another handle is live"),
        Err(refused) => EmbedError::from(refused),
    };
    assert!(matches!(err, EmbedError::SessionStillInUse));

    // Once the other handle is gone, the sole remaining handle parks cleanly.
    drop(live_clone);
    let parked = Box::pin(core.session("busy").created().await.open().await?.park()).await?;
    assert_eq!(parked.session_id(), "busy");
    Ok(())
}

#[tokio::test]
async fn explicit_provider_persists_reopens_and_runs_second_turn() -> Result<()> {
    let backend = double_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let first = core
        .session("provider-reload")
        .created()
        .await
        .open()
        .await?;
    first.send(TurnInput::text("first")).output().await?;
    drop(first);

    let reopened = core
        .session("provider-reload")
        .created()
        .await
        .open()
        .await?;
    let second = reopened.send(TurnInput::text("second")).output().await?;

    assert_eq!(assistant_prose(&second.activities), "echo: second");
    assert_eq!(
        reopened
            .policy_snapshot()
            .model_key()
            .map(ToString::to_string),
        Some("mock-model".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn core_delete_session_removes_factory_backed_session_state() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend_explicit_reconcile().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("delete-session")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("stored before delete"))
        .output()
        .await?;
    assert!(!session.read_view().messages().is_empty());
    assert!(
        !core
            .session("delete-session")
            .durable()
            .await?
            .was_deleted()
            .await?
    );
    drop(session);

    let report = delete_bound_session(&core, "delete-session").await?;
    // The tombstone the factory now keeps is the answer a resume needs; a
    // reopened-but-empty session is not on its own evidence that the id is dead.
    assert!(
        core.session("delete-session")
            .durable()
            .await?
            .was_deleted()
            .await?
    );
    assert!(
        !core
            .session("never-existed")
            .durable()
            .await?
            .was_deleted()
            .await?
    );
    // Ids are single-use: the tombstone refuses a reopen rather than handing
    // back an empty session under the deleted id.
    let reopen = core.session("delete-session").created().await.open().await;
    assert!(
        matches!(
            &reopen,
            Err(EmbedError::Store(
                lash_core::StoreError::SessionDeleted { .. }
            ))
        ),
        "a deleted id must not reopen"
    );

    assert_eq!(report.session_id, "delete-session");
    Ok(())
}

#[tokio::test]
async fn public_session_state_appends_preserve_concurrent_retirement_refusals() -> Result<()> {
    let backend = double_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    for (session_id, append_plugin_body) in [
        ("retired-append-messages", false),
        ("retired-append-plugin-body", true),
    ] {
        let session = core.session(session_id).created().await.open().await?;
        factory
            .delete_session(&SessionId::from(session_id))
            .await
            .expect("retire session before public state append");

        let error =
            if append_plugin_body {
                Box::pin(
                    session
                        .admin()
                        .state()
                        .append_plugin_body("test-plugin", serde_json::json!({ "retired": true })),
                )
                .await
                .expect_err("plugin-body append must preserve the retirement refusal")
            } else {
                Box::pin(session.admin().state().append_messages(vec![
                    lash_core::PluginMessage::text(lash_core::MessageRole::User, "must not append"),
                ]))
                .await
                .expect_err("message append must preserve the retirement refusal")
            };

        // A host append is a session command (FIG-4202): the retired
        // session refuses its submission, typed, before anything is queued.
        assert!(
            matches!(
                &error,
                EmbedError::Runtime(runtime)
                    if runtime.code == lash_core::RuntimeErrorCode::SessionDeleted
                        && matches!(
                            &runtime.cause,
                            Some(lash_core::RuntimeErrorCause::SessionDeleted {
                                session_id: deleted_session_id,
                            }) if deleted_session_id == session_id
                        )
            ),
            "{error:?}"
        );
        assert!(
            error.to_string().contains(
                &lash_core::StoreError::SessionDeleted {
                    session_id: SessionId::from(session_id),
                }
                .to_string()
            ),
            "{error}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn open_with_state_uses_manual_state_and_persists_tool_state() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("manual-state"),
        policy: lash_core::SessionPolicy {
            model: Some(recorded_model(mock_model_spec())),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
        },
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "manual input",
    )]);
    let backend = double_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;

    let created = core.session("manual-state").created().await;
    // A complete state carries the plugin configuration its session recorded
    // (FIG-4398): a rebuilt session that recorded none is refused.
    state.authority.plugin_config = lash_core::SessionCommitStore::load_session_head_meta(
        core.store_factory.as_ref(),
        &SessionId::from("manual-state"),
    )
    .await?
    .expect("the created session's head")
    .config
    .plugin_config;
    let opened = created.open_with_state(state).await?;
    assert_eq!(
        message_text(&opened.read_view().messages().to_vec()[0]),
        "manual input"
    );
    opened
        .admin()
        .tools()
        .set_membership("tool:app_lookup", false)
        .await?;
    let mut persisted = opened.admin().state().persist_current().await?;
    let expected_generation = opened
        .admin()
        .tools()
        .state()
        .await?
        .generation()
        .saturating_add(5);
    persisted.set_tool_state_snapshot(Some(persisted_tool_state_at_generation(
        opened.admin().tools().state().await?,
        expected_generation,
    )));
    drop(opened);

    let reopened = core
        .session("manual-state")
        .created()
        .await
        .open_with_state(persisted)
        .await?;
    let state = reopened.admin().tools().state().await?;
    assert_eq!(state.generation(), expected_generation);
    assert!(
        !state
            .get(&lash_core::ToolId::from("tool:app_lookup"))
            .expect("app tool")
            .is_member(),
        "the host-removed tool is restored as a non-member"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
/// FIG-4099: a model change is a config patch, and the patched model reaches
/// every runtime consumer. (It used to be a reopen that stated the model.)
async fn a_patched_model_reaches_all_runtime_consumers() -> Result<()> {
    use lash_subagents::Capability as _;

    let session_id = "reconcile-open";
    let builder_model = model_spec("builder-model", None, 77_777);
    let persisted = conflicting_reopen_state(&SessionId::from(session_id));
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let (backend, _) = backend_seeded(persisted).await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_probe = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let request_probe = Arc::clone(&request_probe);
            async move {
                request_probe
                    .lock_recover()
                    .push(request.model);
                let response_index = request_probe
                    .lock_recover()
                    .len();
                if response_index == 1 {
                    Ok(text_response(&typescript_block(
                        r#"await control.continue_as({ task: "continue under reconciled policy" });"#,
                    )))
                } else {
                    Ok(text_response(&typescript_block(
                        r#"finish("reconciled");"#,
                    )))
                }
            }
        })
        .build()
        .into_handle();
    let transform_observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transform = Arc::new(ReconciliationTransformProbe {
        observations: Arc::clone(&transform_observations),
    });
    let probe_factory = Arc::new(ReconciliationProbeFactory { transform });
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .models(test_catalog(
            provider,
            [
                builder_model.clone(),
                model_spec("top-level-model", None, 33_333),
                model_spec("current-frame-model", None, 22_222),
                model_spec("historical-model", None, 11_111),
            ],
        ))
        .model("builder-model")
        .plugin(probe_factory)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).created().await.open().await?;
    assert_eq!(
        session.policy_snapshot().wire_model(),
        Some("top-level-model"),
        "the reopen runs the recorded model"
    );
    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetModel {
                model: lash_core::ModelKey::new("builder-model"),
            },
        ))
        .await?;

    let policy = session.policy_snapshot();
    assert_eq!(policy.model, Some(recorded_model(builder_model.clone())));
    println!(
        "consumer 1 policy_snapshot: model={} context_window_tokens={}",
        policy.wire_model().unwrap_or_default(),
        policy.context_window_tokens().unwrap_or_default()
    );

    session
        .send(TurnInput::text("verify reconciliation"))
        .output()
        .await?;

    let observations = transform_observations.lock_recover().clone();
    assert!(!observations.is_empty());
    assert!(observations.iter().all(|observation| {
        observation.max_context_tokens == Some(77_777)
            && observation.session_model == "builder-model"
    }));
    println!(
        "consumer 2 TurnTransformContext.max_context_tokens: {:?}",
        observations[0].max_context_tokens
    );
    println!(
        "consumer 4 context.sessions().model(): {}",
        observations[0].session_model
    );

    let requests = requests.lock_recover().clone();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|model| model == "builder-model"));
    println!("consumer 3 primary LlmRequest.model: {}", requests[0]);

    let writer = session.runtime.writer();
    let mut runtime = writer.lock().await;
    let state = runtime
        .export_persisted_state()
        .await
        .expect("export persisted state");
    drop(runtime);
    // The historical frame is durable, not resident: read it from the
    // session's ancestry.
    let durable_frames = crate::tests::history_frames(
        crate::tests::durable_history(&session.durable()).await?,
        &state.session_id,
    );
    let historical = durable_frames
        .iter()
        .find(|frame| frame.frame_node_id == historical_frame_id)
        .expect("historical frame remains");
    assert_eq!(
        historical.assignment.policy.wire_model(),
        Some("historical-model")
    );
    let current = state.current_agent_frame().expect("current follow frame");
    assert_eq!(
        current.assignment.policy.model,
        Some(recorded_model(builder_model.clone()))
    );

    let tier = lash_subagents::TierCapability::new(
        "inherited",
        None,
        lash_subagents::ChildPluginSource::ParentFork,
    );
    let parent_snapshot = state.to_snapshot();
    let session_spec = lash_core::facade_support::SessionSpec::inherit();
    let tool_access = lash_core::SessionToolAccess::default();
    let child = tier
        .build_session_request(lash_subagents::SubagentSpawnContext {
            parent_session_id: &SessionId::from(session_id),
            parent_snapshot: &parent_snapshot,
            session_spec: &session_spec,
            base_tool_access: &tool_access,
            final_answer_format: lash_subagents::RlmFinalAnswerFormat::RawFinalValue,
            output_schema: None,
            seed: Default::default(),
            parent_subagent: None,
            caused_by: None,
        })
        .expect("inherited child request");
    let child_policy = child.policy.expect("child policy");
    assert_eq!(
        child_policy.model,
        Some(recorded_model(builder_model.clone()))
    );
    println!(
        "consumer 5 child tier inheritance: model={} context_window_tokens={}",
        child_policy.wire_model().unwrap_or_default(),
        child_policy.context_window_tokens().unwrap_or_default()
    );

    let execution_env = state.process_execution_env_spec(&policy);
    assert_eq!(
        execution_env.policy.model,
        Some(recorded_model(builder_model.clone()))
    );
    println!(
        "consumer 6 ProcessExecutionEnvSpec.policy: model={} context_window_tokens={}",
        execution_env.policy.wire_model().unwrap_or_default(),
        execution_env
            .policy
            .context_window_tokens()
            .unwrap_or_default()
    );
    println!(
        "consumer 7 continue_as follow frame: model={} context_window_tokens={}; historical_frame_model={}",
        current.assignment.policy.wire_model().unwrap_or_default(),
        current
            .assignment
            .policy
            .context_window_tokens()
            .unwrap_or_default(),
        historical
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default()
    );
    Ok(())
}

#[tokio::test]
async fn open_with_state_keeps_supplied_policy_without_rewriting_frame_history() -> Result<()> {
    let session_id = "reconcile-open-with-state";
    let persisted = conflicting_reopen_state(&SessionId::from(session_id));
    let supplied_model = persisted.policy.model.clone();
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let builder_model = model_spec("builder-model", None, 77_777);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), builder_model.clone())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session(session_id)
        .created()
        .await
        .open_with_state(persisted)
        .await?;
    let writer = session.runtime.writer();
    let state = writer
        .lock()
        .await
        .export_persisted_state()
        .await
        .expect("export persisted state");
    // The spec named no model, so the supplied state's model survives:
    // core defaults are construction fallbacks, not per-open seeds.
    assert_eq!(state.policy.model, supplied_model);
    assert_eq!(
        state
            .current_agent_frame()
            .expect("current frame")
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default(),
        "current-frame-model"
    );
    assert_eq!(
        state
            .agent_frames
            .iter()
            .find(|frame| frame.frame_node_id == historical_frame_id)
            .expect("historical frame")
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default(),
        "historical-model"
    );
    Ok(())
}

#[tokio::test]
async fn queued_worker_state_load_keeps_durable_policy_without_rewriting_history() -> Result<()> {
    let session_id = "reconcile-queued-worker";
    let persisted = conflicting_reopen_state(&SessionId::from(session_id));
    let durable_model = persisted.policy.model.clone();
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let (_backend, store) = backend_seeded(persisted).await;
    let policy = lash_core::SessionPolicy {
        model: Some(recorded_model(model_spec("builder-model", None, 77_777))),
        session_id: Some(SessionId::from(session_id)),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };

    let state =
        crate::session::load_state_from_store(&SessionId::from(session_id), &policy, &store)
            .await?;
    // A stateless worker's load carries no host spec at all: the durable
    // head's recorded model is authoritative over the resolved fallback.
    assert_eq!(state.policy.model, durable_model);
    assert_eq!(
        state
            .current_agent_frame()
            .expect("current frame")
            .assignment
            .policy
            .wire_model()
            .unwrap_or_default(),
        "current-frame-model"
    );
    // The load does not rewrite history: the historical frame's durable
    // record keeps its own model.
    assert_eq!(
        crate::tests::history_frames(
            crate::tests::store_history(&store).await?,
            &state.session_id
        )
        .iter()
        .find(|frame| frame.frame_node_id == historical_frame_id)
        .expect("historical frame")
        .assignment
        .policy
        .wire_model()
        .unwrap_or_default(),
        "historical-model"
    );
    Ok(())
}

#[tokio::test]
async fn core_store_factory_is_used_for_sessions_created_from_a_running_session() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core
        .session("root-with-child-store")
        .created()
        .await
        .open()
        .await?;

    core.session("child-store")
        .create(crate::SessionCreation {
            parent: Some("root-with-child-store".into()),
            ..Default::default()
        })
        .await?;
    core.session("child-store").open().await?;

    let mut session_ids = core
        .sessions()
        .await?
        .into_iter()
        .map(|summary| summary.session_id)
        .collect::<Vec<_>>();
    session_ids.sort();
    assert_eq!(
        session_ids,
        vec![
            SessionId::from("child-store"),
            SessionId::from("root-with-child-store"),
        ],
        "both sessions live in the backend's one catalog"
    );
    Ok(())
}
