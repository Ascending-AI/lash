use super::*;
use crate::rlm::{RlmFinalAnswerFormat, RlmSessionBuilderExt as _, RlmTurnBuilderExt as _};
use lash_lashlang_runtime::LashlangArtifactStore as _;

fn persisted_tool_state_at_generation(
    state: lash_core::ToolState,
    generation: u64,
) -> lash_core::ToolState {
    let mut value = serde_json::to_value(state).expect("serialize persisted tool state");
    value["generation"] = serde_json::json!(generation);
    serde_json::from_value(value).expect("deserialize persisted tool state")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReconciliationTransformObservation {
    max_context_tokens: Option<usize>,
    session_model: String,
}

struct ReconciliationTransformProbe {
    observations: Arc<std::sync::Mutex<Vec<ReconciliationTransformObservation>>>,
}

struct ReconciliationProbeFactory {
    transform: Arc<dyn lash_core::TurnContextTransform>,
}

impl lash_core::PluginFactory for ReconciliationProbeFactory {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(ReconciliationProbePlugin {
            transform: Arc::clone(&self.transform),
        }))
    }
}

struct ReconciliationProbePlugin {
    transform: Arc<dyn lash_core::TurnContextTransform>,
}

impl lash_core::SessionPlugin for ReconciliationProbePlugin {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    fn register(
        &self,
        reg: &mut lash_core::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.context().prepare_turn(0, Arc::clone(&self.transform));
        Ok(())
    }
}

#[async_trait]
impl lash_core::TurnContextTransform for ReconciliationTransformProbe {
    fn id(&self) -> &'static str {
        "session-model-reconciliation-probe"
    }

    async fn transform(
        &self,
        ctx: &lash_core::TurnTransformContext<'_>,
        input: lash_core::PreparedContext,
    ) -> std::result::Result<lash_core::PreparedContext, lash_core::ContextError> {
        let snapshot = ctx.sessions.snapshot_session(&ctx.session_id).await?;
        self.observations
            .lock()
            .expect("reconciliation observations")
            .push(ReconciliationTransformObservation {
                max_context_tokens: ctx.max_context_tokens,
                session_model: snapshot.policy.model.id,
            });
        Ok(input)
    }
}

fn conflicting_reopen_state(session_id: &str) -> RuntimeSessionState {
    let historical_policy = lash_core::SessionPolicy {
        provider_id: "persisted-provider".to_string(),
        model: model_spec("historical-model", None, 11_111),
        ..Default::default()
    };
    let current_policy = lash_core::SessionPolicy {
        provider_id: "persisted-provider".to_string(),
        model: model_spec("current-frame-model", None, 22_222),
        ..Default::default()
    };
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        policy: historical_policy.clone(),
        agent_frames: Vec::new(),
        current_frame_node_id: None,
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let frame_node_id = format!("agent-frame:{session_id}:current");
    let mut nodes = state.session_graph.nodes.clone();
    nodes.push(lash_core::SessionNodeRecord {
        node_id: frame_node_id.clone(),
        parent_node_id: state.session_graph.leaf_node_id.clone(),
        timestamp: "2026-07-27T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key: format!("conflicting-frame-{session_id}"),
            reason: lash_core::AgentFrameReason::continue_as(),
            assignment: lash_core::AgentFrameAssignment::from_policy(current_policy),
            protocol_turn_options: Default::default(),
        },
    });
    state.session_graph = lash_core::SessionGraph::from_nodes(nodes, Some(frame_node_id.clone()));
    state.current_frame_node_id = Some(frame_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(session_id);
    state.policy = lash_core::SessionPolicy {
        provider_id: "persisted-provider".to_string(),
        model: model_spec("top-level-model", None, 33_333),
        ..Default::default()
    };
    state
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
struct CompileSurfaceToolConfig {
    tool_name: String,
}

struct CompileSurfaceToolFactory {
    id: &'static str,
    default_tool_name: &'static str,
}

impl CompileSurfaceToolFactory {
    fn new(id: &'static str, default_tool_name: &'static str) -> Self {
        Self {
            id,
            default_tool_name,
        }
    }
}

impl lash_core::PluginFactory for CompileSurfaceToolFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(
        &self,
        ctx: &lash_core::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::SessionPlugin>, lash_core::PluginError> {
        let config = ctx
            .plugin_options
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

struct CompileSurfaceToolPlugin {
    plugin_id: &'static str,
    tool_name: String,
}

impl lash_core::SessionPlugin for CompileSurfaceToolPlugin {
    fn id(&self) -> &'static str {
        self.plugin_id
    }

    fn register(
        &self,
        reg: &mut lash_core::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.tools().provider(Arc::new(CompileSurfaceToolProvider {
            tool_name: self.tool_name.clone(),
        }))?;
        Ok(())
    }
}

struct CompileSurfaceToolProvider {
    tool_name: String,
}

#[async_trait]
impl lash_core::ToolProvider for CompileSurfaceToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![compile_surface_tool_definition(&self.tool_name).manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == self.tool_name)
            .then(|| Arc::new(compile_surface_tool_definition(&self.tool_name).contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        lash_core::ToolResult::ok(serde_json::json!({ "ok": true }))
    }
}

fn compile_surface_tool_definition(name: &str) -> lash_core::ToolDefinition {
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
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        name.to_string(),
    ))
}

#[tokio::test]
async fn standard_core_runs_mock_turn() -> Result<()> {
    let core = standard_core();
    let session = core.session("main").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("hello"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. })
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

#[tokio::test]
async fn commit_byte_budget_failure_reaches_the_host_as_terminal_and_actionable() -> Result<()> {
    let oversized_text =
        "x".repeat(lash_core::RuntimeCommit::MAX_COMMIT_BUDGET_BYTES.saturating_add(1));
    let provider = crate::testing::TestProvider::builder()
        .kind("oversized-commit")
        .complete(move |_request| {
            let oversized_text = oversized_text.clone();
            async move { Ok(text_response(&oversized_text)) }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;
    let session = core.session("commit-budget-surface").open().await?;

    let error = match session
        .turn(TurnInput::text("produce an oversized turn"))
        .run()
        .await
    {
        Ok(_) => panic!("the oversized turn must fail at the production surface"),
        Err(error) => error,
    };

    let EmbedError::Runtime(runtime_error) = &error else {
        panic!("expected a host-visible runtime error, got {error}");
    };
    assert_eq!(
        runtime_error.code,
        lash_core::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert!(
        runtime_error.message.contains(&format!(
            "exceeding the {}-byte transaction budget",
            lash_core::RuntimeCommit::MAX_COMMIT_BUDGET_BYTES
        )),
        "{}",
        runtime_error.message
    );
    assert!(error.is_terminal(), "{error}");
    assert!(!error.is_retryable(), "{error}");
    Ok(())
}

#[test]
fn typed_core_builders_require_explicit_store_choice() {
    let err = match LashCore::standard_builder()
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()
    {
        Ok(_) => panic!("standard preset must not install implicit in-memory stores"),
        Err(err) => err,
    };
    assert!(matches!(err, EmbedError::MissingEffectHost));

    let err = match LashCore::standard_builder()
        .provider(mock_provider())
        .model(mock_model_spec())
        .effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
        .build()
    {
        Ok(_) => panic!("attachment store must be explicit after effect host is wired"),
        Err(err) => err,
    };
    assert!(matches!(err, EmbedError::MissingAttachmentStore));

    // The RLM factory now requires the Lashlang artifact store at construction,
    // so a missing-artifact-store error is unrepresentable. The RLM preset still
    // must not install implicit generic stores.
    let err = match rlm_core_builder()
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()
    {
        Ok(_) => panic!("rlm preset must not install implicit generic stores"),
        Err(err) => err,
    };
    assert!(matches!(err, EmbedError::MissingEffectHost));
}

#[test]
fn generic_lash_core_builder_requires_protocol_plugin() {
    let err = match explicit_ephemeral_facets(LashCore::builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()
    {
        Ok(_) => panic!("generic LashCore must require an explicit protocol plugin"),
        Err(err) => err,
    };

    assert!(matches!(err, EmbedError::MissingProtocolPlugin));
}

#[tokio::test]
async fn prompt_layers_apply_across_core_session_turn_and_mutation_scopes() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(recording_prompt_provider(Arc::clone(&seen)))
        .model(mock_model_spec())
        .prompt_contribution(PromptContribution::guidance("Core", "core guidance"))
        .build()?;
    let session = core
        .session("prompt-api")
        .prompt_contribution(PromptContribution::guidance("Session", "session guidance"))
        .open()
        .await?;

    session
        .turn(TurnInput::text("first"))
        .prompt_contribution(PromptContribution::guidance("Turn", "turn guidance"))
        .run()
        .await?;
    session
        .admin()
        .config()
        .replace_prompt_slot(
            PromptSlot::Guidance,
            [PromptContribution::guidance(
                "Replacement",
                "replacement guidance",
            )],
        )
        .await?;
    session.turn(TurnInput::text("second")).run().await?;
    session
        .admin()
        .config()
        .clear_prompt_slot(PromptSlot::Guidance)
        .await?;
    session.turn(TurnInput::text("third")).run().await?;

    let prompts = seen.lock().expect("seen prompts");
    assert!(prompts[0].contains("core guidance"));
    assert!(prompts[0].contains("session guidance"));
    assert!(prompts[0].contains("turn guidance"));
    assert!(prompts[1].contains("replacement guidance"));
    assert!(!prompts[1].contains("core guidance"));
    assert!(!prompts[1].contains("session guidance"));
    assert!(!prompts[2].contains("core guidance"));
    assert!(!prompts[2].contains("replacement guidance"));
    Ok(())
}

#[tokio::test]
async fn provider_overrides_apply_at_core_session_turn_and_config_scopes() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(text_provider("core-provider", "core-model", "core"))
        .model(model_spec("core-model", None, 200_000))
        .build()
        .expect("standard core");
    let session = core
        .session("main")
        .provider(text_provider(
            "session-provider",
            "session-model",
            "session",
        ))
        .open()
        .await?;

    let session_result = session.turn(TurnInput::text("hello")).run().await?;
    assert_eq!(assistant_prose(&session_result.activities), "session");

    let turn_result = session
        .turn(TurnInput::text("hello"))
        .provider(text_provider("turn-provider", "turn-model", "turn"))
        .run()
        .await?;
    assert_eq!(assistant_prose(&turn_result.activities), "turn");

    let after_turn = session.turn(TurnInput::text("hello")).run().await?;
    assert_eq!(assistant_prose(&after_turn.activities), "session");

    session
        .admin()
        .config()
        .update(SessionConfigPatch {
            provider: Some(text_provider(
                "updated-provider",
                "updated-model",
                "updated",
            )),
            model: Some(model_spec("updated-model", None, 200_000)),
            ..SessionConfigPatch::default()
        })
        .await?;

    let updated = session.turn(TurnInput::text("hello")).run().await?;
    assert_eq!(assistant_prose(&updated.activities), "updated");
    Ok(())
}

#[tokio::test]
async fn provider_only_overrides_keep_session_model_and_variant() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(recording_text_provider(
            "core-provider",
            "core-model",
            Some("core-variant"),
            "core",
            Arc::clone(&seen),
        ))
        .model(model_spec(
            "core-model",
            Some("core-variant".to_string()),
            200_000,
        ))
        .build()
        .expect("standard core");
    let session = core
        .session("main")
        .provider(recording_text_provider(
            "session-provider",
            "session-model",
            Some("session-variant"),
            "session",
            Arc::clone(&seen),
        ))
        .open()
        .await?;

    session.turn(TurnInput::text("hello")).run().await?;
    session
        .turn(TurnInput::text("hello"))
        .provider(recording_text_provider(
            "turn-provider",
            "turn-model",
            Some("turn-variant"),
            "turn",
            Arc::clone(&seen),
        ))
        .run()
        .await?;
    session
        .admin()
        .config()
        .update(SessionConfigPatch {
            provider: Some(recording_text_provider(
                "updated-provider",
                "updated-model",
                Some("updated-variant"),
                "updated",
                Arc::clone(&seen),
            )),
            ..SessionConfigPatch::default()
        })
        .await?;
    session.turn(TurnInput::text("hello")).run().await?;

    assert_eq!(
        *seen.lock().expect("seen requests"),
        vec![
            (
                "core-model".to_string(),
                lash_core::ReasoningSelection::Effort("core-variant".to_string()),
            ),
            (
                "core-model".to_string(),
                lash_core::ReasoningSelection::Effort("core-variant".to_string()),
            ),
            (
                "core-model".to_string(),
                lash_core::ReasoningSelection::Effort("core-variant".to_string()),
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn rlm_core_opens_rlm_session() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;

    core.session("rlm").open().await?;
    Ok(())
}

#[tokio::test]
async fn rlm_protocol_config_lashlang_abilities_drive_prompt_surface() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-abilities-prompt-test")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock()
                        .expect("seen prompts")
                        .push(system_text(&request));
                    Ok(text_response(&lashlang_block("finish \"ok\"")))
                }
            }
        })
        .build()
        .into_handle();
    let config: crate::rlm::RlmProtocolPluginConfig = serde_json::from_value(serde_json::json!({
        "lashlang_abilities": { "processes": true, "triggers": true }
    }))
    .expect("rlm config");
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(config, inmem_artifact_store());
    let core = LashCore::rlm_builder(factory)
        .provider(provider)
        .model(mock_model_spec())
        .effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(crate::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            crate::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("rlm-abilities-prompt").open().await?;

    session
        .turn(TurnInput::text("hello"))
        .require_finish()?
        .run()
        .await?;

    let prompts = seen.lock().expect("seen prompts");
    assert!(prompts[0].contains("Trigger registry"));
    assert!(prompts[0].contains("trigger registration connects"));
    assert!(prompts[0].contains("process definition"));
    assert!(prompts[0].contains("triggers.list({})"));
    assert!(!prompts[0].contains("TRIGGER."));
    Ok(())
}

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
                        let mut seen = seen.lock().expect("seen requests");
                        seen.push(request_text(&request));
                        seen.len()
                    };
                    let answer = match request_index {
                        1 => ANSWER,
                        2 => "second answer",
                        other => panic!("unexpected provider request {other}"),
                    };
                    Ok(text_response(&lashlang_block(&format!(
                        "finish {answer:?}"
                    ))))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .trace_jsonl_path(trace_path.clone())
        .build()?;
    let session = core
        .session("rlm-finish-history-single-copy")
        .open()
        .await?;

    session
        .turn(TurnInput::text("first"))
        .require_finish()?
        .run()
        .await?;
    session
        .admin()
        .state()
        .append_messages(vec![
            lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, ANSWER)
                .with_id("workbench-assistant:fig-461-turn-1"),
        ])
        .await?;
    session
        .turn(TurnInput::text("second"))
        .require_finish()?
        .run()
        .await?;
    core.flush_trace_sink()?;

    let seen = seen.lock().expect("seen requests");
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].matches(ANSWER).count(),
        1,
        "the committed terminal answer must occur exactly once in turn 2"
    );
    let trace = std::fs::read_to_string(&trace_path).expect("read FIG-461 trace");
    let llm_starts = trace
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event JSON"))
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
                                text.contains(&lashlang_block(&format!(
                                    "print \"turn {turn} observation\""
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
                        lashlang_block(&format!("print \"turn {} observation\"", turn + 1))
                    } else {
                        lashlang_block(&format!("finish {:?}", ANSWERS[turn]))
                    };
                    Ok(text_response(&response))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .build()?;
    let session = core.session("rlm-multi-turn-history-shape").open().await?;

    for (turn, answer) in ANSWERS.iter().enumerate() {
        session
            .turn(TurnInput::text(format!("turn {}", turn + 1)))
            .require_finish()?
            .run()
            .await?;
        session
            .admin()
            .state()
            .append_messages(vec![
                lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, *answer)
                    .with_id(format!("workbench-assistant:few-shot-turn-{}", turn + 1)),
            ])
            .await?;
    }

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    Ok(())
}

#[tokio::test]
async fn rlm_compile_surface_uses_core_plugins_extra_plugins_and_request_options() -> Result<()> {
    // The compile APIs are now operations over the RLM factory and a plugin host
    // the caller builds. The plugin host carries the core tool plugin plus any
    // extra tool plugins; the request's execution env plugin options configure
    // them (here `compile-extra-tool` resolves to `lookup`).
    let artifact_store = Arc::new(crate::persistence::InMemoryLashlangArtifactStore::new());
    let factory = Arc::new(lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        artifact_store.clone(),
    ));
    let plugin_host = lash_core::PluginHost::new(vec![
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
    let plugin_options = || {
        lash_core::PluginOptions::typed(
            "compile-extra-tool",
            CompileSurfaceToolConfig {
                tool_name: "lookup".to_string(),
            },
        )
        .expect("compile plugin options serialize")
    };
    let request = crate::rlm::LashlangCompileSurfaceRequest::new(
        "compile-surface",
        lash_core::ProcessExecutionEnvSpec::new(
            plugin_options(),
            lash_core::SessionPolicy::default(),
        ),
    );

    let surface =
        factory.lashlang_compile_surface(&plugin_host, process_lifecycle_available, request)?;

    assert!(surface.host_environment.abilities.processes);
    assert!(surface.host_environment.abilities.sleep);
    assert!(surface.host_environment.abilities.process_signals);
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
value = tools.lookup({})
finish value
"#,
                lash_core::ProcessExecutionEnvSpec::new(
                    plugin_options(),
                    lash_core::SessionPolicy::default(),
                ),
            ),
        )
        .await
        .expect("compile module through the RLM factory");
    assert!(
        artifact_store
            .get_module_artifact(&compiled.module_ref)
            .await
            .expect("load persisted module artifact")
            .is_some(),
        "compile_lashlang_module should persist through the configured artifact store"
    );
    Ok(())
}

#[tokio::test]
async fn rlm_root_session_final_answer_format_defaults_to_markdown_and_can_be_raw() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(recording_request_provider(Arc::clone(&seen)))
        .model(mock_model_spec())
        .build()?;

    let markdown = core.session("rlm-root-markdown").open().await?;
    markdown.turn(TurnInput::text("hello")).run().await?;

    let raw = core
        .session("rlm-root-raw")
        .final_answer_format(RlmFinalAnswerFormat::RawFinalValue)?
        .open()
        .await?;
    raw.turn(TurnInput::text("hello"))
        .require_finish()?
        .run()
        .await?;

    let prompts = seen.lock().expect("seen prompts");
    assert!(prompts[0].contains("=== FINAL ANSWER FORMAT ==="));
    assert!(prompts[0].contains("Markdown string"));
    assert!(!prompts[1].contains("=== FINAL ANSWER FORMAT ==="));
    assert!(!prompts[1].contains("Markdown string"));
    Ok(())
}

#[tokio::test]
async fn malformed_rlm_create_extras_fail_child_session_creation() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("rlm-root").open().await?;
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

    let err = session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("rlm-child-bad-extras".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "rlm-root".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        })
        .await
        .expect_err("malformed RLM create extras should fail session creation");

    assert!(err.to_string().contains("invalid RLM create options"));
    Ok(())
}

#[tokio::test]
async fn rlm_projection_errors_surface_from_protocol_extensions() -> Result<()> {
    use lash_protocol_rlm::{RlmProjectedBindings, RlmTurnInputExt};

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let session = core.session("rlm").open().await?;
    session
        .admin()
        .protocol()
        .apply_session_extension(lash_protocol_rlm::rlm_session_projection_extension(
            RlmProjectedBindings::new()
                .bind_json("current_query", serde_json::json!("session"))
                .expect("session bind"),
        ))
        .await?;

    let input = TurnInput::text("hello")
        .rlm_project(
            RlmProjectedBindings::new()
                .bind_json("current_query", serde_json::json!("turn"))
                .expect("turn bind"),
        )
        .map_err(|err| EmbedError::Session(SessionError::Protocol(err.to_string())))?;
    let err = match session.turn(input).run().await {
        Ok(_) => panic!("duplicate session and turn projection should fail"),
        Err(err) => err,
    };
    assert!(
        matches!(err, EmbedError::Session(message) if message.to_string().contains("current_query"))
    );
    Ok(())
}

#[tokio::test]
async fn store_factory_reopens_persisted_session_state() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "persisted".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "already stored",
    )]);
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .build()?;

    let reopened = core.session("persisted").open().await?;
    let messages = reopened.read_view().messages().to_vec();
    assert_eq!(messages.len(), 1);
    assert_eq!(message_text(&messages[0]), "already stored");
    Ok(())
}

#[tokio::test]
async fn park_then_resume_preserves_session_transcript() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;

    let session = core.session("parked").open().await?;
    session.turn(TurnInput::text("hello")).run().await?;
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
    let parked = session.park().await?;
    assert_eq!(parked.session_id(), "parked");

    // Resume rebuilds a live session; the flushed transcript is visible again.
    let resumed = core.resume(parked).await?;
    let after = resumed
        .read_view()
        .messages()
        .iter()
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(after, before, "resume must restore the parked transcript");
    // The resumed session is live and can take another turn on top of the
    // restored transcript.
    resumed.turn(TurnInput::text("again")).run().await?;
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

#[tokio::test]
async fn park_with_a_live_handle_reports_session_still_in_use() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;

    let session = core.session("busy").open().await?;
    // A live clone shares the underlying runtime handle, exactly as an in-flight
    // turn would: parking must refuse rather than silently flush a session that
    // something else is still driving.
    let live_clone = session.clone();
    let err = match session.park().await {
        Ok(_) => panic!("park must not proceed while another handle is live"),
        Err(err) => err,
    };
    assert!(matches!(err, EmbedError::SessionStillInUse));

    // Once the other handle is gone, the sole remaining handle parks cleanly.
    drop(live_clone);
    let parked = core.session("busy").open().await?.park().await?;
    assert_eq!(parked.session_id(), "busy");
    Ok(())
}

#[test]
fn session_policy_serializes_provider_id_without_provider_config() -> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("secret-provider")
        .serialize_config(|| serde_json::json!({ "api_key": "should-not-persist" }))
        .build()
        .into_handle();
    let policy = lash_core::SessionPolicy {
        provider_id: provider.kind().to_string(),
        model: mock_model_spec(),
        ..Default::default()
    };

    let value = serde_json::to_value(&policy)?;
    assert_eq!(value["provider_id"], "secret-provider");
    assert!(value.get("provider").is_none());
    assert!(!value.to_string().contains("should-not-persist"));

    let decoded: lash_core::SessionPolicy = serde_json::from_value(value)?;
    assert_eq!(decoded.recorded_provider_id(), "secret-provider");
    Ok(())
}

#[tokio::test]
async fn persisted_provider_id_rebinds_to_live_provider_on_open() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "provider-rebind".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: "embed-test".to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        current_frame_node_id: None,
        agent_frames: Vec::new(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "stored",
    )]);
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .build()?;

    let reopened = core.session("provider-rebind").open().await?;
    let persisted = reopened.admin().state().persist_current().await?;

    assert_eq!(persisted.policy.recorded_provider_id(), "embed-test");
    assert!(
        persisted
            .agent_frames
            .iter()
            .all(|frame| frame.assignment.policy.recorded_provider_id() == "embed-test")
    );
    Ok(())
}

#[tokio::test]
async fn persisted_provider_id_mismatch_fails_at_turn_execution() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "provider-mismatch".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: "other-provider".to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        current_frame_node_id: None,
        agent_frames: Vec::new(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .build()?;

    let session = core.session("provider-mismatch").open().await?;
    let err = match session.turn(TurnInput::text("must not run")).run().await {
        Ok(_) => panic!("provider mismatch should fail at turn execution"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        EmbedError::Runtime(lash_core::RuntimeError {
            code: lash_core::RuntimeErrorCode::Other(code),
            message,
        }) if code == "llm_provider"
            && message.contains("other-provider")
            && message.contains("provider-mismatch")
    ));
    Ok(())
}

#[tokio::test]
async fn agent_frame_provider_id_mismatch_is_reconciled_on_open() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "frame-provider-mismatch".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: "embed-test".to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        current_frame_node_id: None,
        agent_frames: Vec::new(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state.session_graph.leaf_node_id.clone();
    let mut nodes = state.session_graph.nodes.clone();
    let frame = nodes
        .iter_mut()
        .find(|node| Some(&node.node_id) == state.current_frame_node_id.as_ref())
        .expect("initial frame node");
    let lash_core::SessionNodePayload::FrameOpen { assignment, .. } = &mut frame.payload else {
        panic!("current frame must be a FrameOpen node");
    };
    assignment.policy.provider_id = "other-provider".to_string();
    state.session_graph = lash_core::SessionGraph::from_nodes(nodes, leaf_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .build()?;

    let session = core.session("frame-provider-mismatch").open().await?;
    assert_eq!(
        session.policy_snapshot().recorded_provider_id(),
        "embed-test"
    );
    session
        .turn(TurnInput::text("runs with reconciled provider"))
        .run()
        .await?;
    Ok(())
}

#[tokio::test]
async fn refreshed_head_provider_id_mismatch_fails_before_turn() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "refresh-provider-mismatch".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: "embed-test".to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        current_frame_node_id: None,
        agent_frames: Vec::new(),
        ..Default::default()
    };
    state.ensure_agent_frame_initialized();
    let store = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let session = core
        .session("refresh-provider-mismatch")
        .store(runtime_store)
        .open()
        .await?;

    store.set_head_provider_id("other-provider");
    let err = match session.turn(TurnInput::text("must not run")).run().await {
        Ok(_) => panic!("head-refresh provider mismatch should fail before turn"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        EmbedError::Runtime(lash_core::RuntimeError {
            code: lash_core::RuntimeErrorCode::Other(code),
            message,
        }) if code == "llm_provider"
            && message.contains("other-provider")
    ));
    Ok(())
}

#[tokio::test]
async fn explicit_provider_persists_reopens_and_runs_second_turn() -> Result<()> {
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()?;

    let first = core
        .session("provider-reload")
        .store(Arc::clone(&store))
        .open()
        .await?;
    first.turn(TurnInput::text("first")).run().await?;
    drop(first);

    let reopened = core
        .session("provider-reload")
        .store(Arc::clone(&store))
        .open()
        .await?;
    let second = reopened.turn(TurnInput::text("second")).run().await?;

    assert_eq!(assistant_prose(&second.activities), "echo: second");
    assert_eq!(
        reopened.policy_snapshot().recorded_provider_id(),
        "embed-test"
    );
    Ok(())
}

#[tokio::test]
async fn core_delete_session_removes_factory_backed_session_state() -> Result<()> {
    let factory = Arc::new(DeletingStoreFactory::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory)
        .build()?;
    let session = core.session("delete-session").open().await?;
    session
        .turn(TurnInput::text("stored before delete"))
        .run()
        .await?;
    assert!(!session.read_view().messages().is_empty());
    drop(session);

    let report = core
        .delete_session("delete-session", session_delete_scope("delete-session"))
        .await?;
    let reopened = core.session("delete-session").open().await?;

    assert_eq!(report.session_id, "delete-session");
    assert!(reopened.read_view().messages().is_empty());
    Ok(())
}

#[tokio::test]
async fn active_path_residency_opens_with_active_path_scope() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "active-path".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut root_message = text_message(lash_core::MessageRole::User, "root");
    root_message.id = "root-message".to_string();
    state.append_active_conversation_messages(&[root_message]);
    let root = state.session_graph.leaf_node_id.clone();
    let mut inactive_message = text_message(lash_core::MessageRole::User, "inactive branch");
    inactive_message.id = "inactive-message".to_string();
    state.append_active_conversation_messages(&[inactive_message]);
    state.session_graph.set_leaf_node_id(root);
    let mut active_message = text_message(lash_core::MessageRole::User, "active branch");
    active_message.id = "active-message".to_string();
    state.append_active_conversation_messages(&[active_message]);

    let store = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory {
            store: store.clone(),
        }))
        .residency(lash_core::Residency::ActivePathOnly)
        .build()?;

    let reopened = core.session("active-path").open().await?;
    let messages = reopened.read_view().messages().to_vec();
    let texts = messages.iter().map(message_text).collect::<Vec<_>>();
    assert_eq!(texts, vec!["root", "active branch"]);
    assert_eq!(
        store.scopes(),
        vec![lash_core::SessionReadScope::ActivePath { leaf_node_id: None }]
    );
    Ok(())
}

#[tokio::test]
async fn keep_all_residency_opens_with_full_graph_scope() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "keep-all".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "already stored",
    )]);
    let store = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory {
            store: store.clone(),
        }))
        .build()?;

    let reopened = core.session("keep-all").open().await?;
    assert_eq!(reopened.read_view().messages().len(), 1);
    assert_eq!(store.scopes(), vec![lash_core::SessionReadScope::FullGraph]);
    Ok(())
}

#[tokio::test]
async fn store_session_id_mismatch_is_rejected() -> Result<()> {
    let state = RuntimeSessionState {
        session_id: "actual-session".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .build()?;

    let err = match core.session("requested-session").open().await {
        Ok(_) => panic!("mismatched store should fail"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        EmbedError::StoreSessionMismatch {
            loaded,
            requested
        } if loaded == "actual-session" && requested == "requested-session"
    ));
    Ok(())
}

#[tokio::test]
async fn open_with_state_uses_manual_state_and_persists_tool_state() -> Result<()> {
    let mut state = RuntimeSessionState {
        session_id: "manual-state".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "manual input",
    )]);
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build()?;

    let opened = core
        .session("manual-state")
        .store(Arc::clone(&store))
        .open_with_state(state)
        .await?;
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
    persisted.tool_state_generation = Some(expected_generation);
    persisted.tool_state_snapshot = Some(persisted_tool_state_at_generation(
        opened.admin().tools().state().await?,
        expected_generation,
    ));
    drop(opened);

    let reopened = core
        .session("manual-state")
        .store(Arc::clone(&store))
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

#[tokio::test]
async fn reopen_reconciles_builder_model_across_all_runtime_consumers() -> Result<()> {
    use lash_subagents::Capability as _;

    let session_id = "reconcile-open";
    let builder_model = model_spec("builder-model", None, 77_777);
    let persisted = conflicting_reopen_state(session_id);
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(SnapshotStore::with_state(persisted));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_probe = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("persisted-provider")
        .complete(move |request| {
            let request_probe = Arc::clone(&request_probe);
            async move {
                request_probe
                    .lock()
                    .expect("reconciliation requests")
                    .push(request.model);
                let response_index = request_probe
                    .lock()
                    .expect("reconciliation requests")
                    .len();
                if response_index == 1 {
                    Ok(text_response(&lashlang_block(
                        r#"await control.continue_as({ task: "continue under reconciled policy" })?"#,
                    )))
                } else {
                    Ok(text_response(&lashlang_block(
                        r#"finish "reconciled""#,
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
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(builder_model.clone())
        .plugin(probe_factory)
        .build()?;
    let session = core
        .session(session_id)
        .store(Arc::clone(&store))
        .open()
        .await?;

    let policy = session.policy_snapshot();
    assert_eq!(policy.model, builder_model);
    println!(
        "consumer 1 policy_snapshot: model={} context_window_tokens={}",
        policy.model.id,
        policy.model.context_window_tokens()
    );

    session
        .turn(TurnInput::text("verify reconciliation"))
        .run()
        .await?;

    let observations = transform_observations
        .lock()
        .expect("reconciliation observations")
        .clone();
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

    let requests = requests.lock().expect("reconciliation requests").clone();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|model| model == "builder-model"));
    println!("consumer 3 primary LlmRequest.model: {}", requests[0]);

    let writer = session.runtime.writer();
    let runtime = writer.lock().await;
    let state = runtime.export_persisted_state();
    drop(runtime);
    let historical = state
        .agent_frames
        .iter()
        .find(|frame| frame.frame_node_id == historical_frame_id)
        .expect("historical frame remains");
    assert_eq!(historical.assignment.policy.model.id, "historical-model");
    let current = state.current_agent_frame().expect("current follow frame");
    assert_eq!(current.assignment.policy.model, builder_model);

    let tier = lash_subagents::TierCapability::new(
        "inherited",
        None,
        lash_subagents::TierPluginSource::CurrentSessionFork,
    );
    let parent_snapshot = state.to_snapshot();
    let session_spec = lash_core::SessionSpec::inherit();
    let tool_access = lash_core::SessionToolAccess::default();
    let child = tier
        .build_session_request(lash_subagents::SubagentSpawnContext {
            parent_session_id: session_id,
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
    assert_eq!(child_policy.model, builder_model);
    println!(
        "consumer 5 child tier inheritance: model={} context_window_tokens={}",
        child_policy.model.id,
        child_policy.model.context_window_tokens()
    );

    let execution_env = state.process_execution_env_spec(&policy);
    assert_eq!(execution_env.policy.model, builder_model);
    println!(
        "consumer 6 ProcessExecutionEnvSpec.policy: model={} context_window_tokens={}",
        execution_env.policy.model.id,
        execution_env.policy.model.context_window_tokens()
    );
    println!(
        "consumer 7 continue_as follow frame: model={} context_window_tokens={}; historical_frame_model={}",
        current.assignment.policy.model.id,
        current.assignment.policy.model.context_window_tokens(),
        historical.assignment.policy.model.id
    );
    Ok(())
}

#[tokio::test]
async fn open_with_state_reconciles_live_policy_without_rewriting_frame_history() -> Result<()> {
    let session_id = "reconcile-open-with-state";
    let persisted = conflicting_reopen_state(session_id);
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let builder_model = model_spec("builder-model", None, 77_777);
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(builder_model.clone())
        .build()?;

    let session = core.session(session_id).open_with_state(persisted).await?;
    let writer = session.runtime.writer();
    let state = writer.lock().await.export_persisted_state();
    assert_eq!(state.policy.model, builder_model);
    assert_eq!(
        state
            .current_agent_frame()
            .expect("current frame")
            .assignment
            .policy
            .model
            .id,
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
            .model
            .id,
        "historical-model"
    );
    Ok(())
}

#[tokio::test]
async fn queued_worker_state_load_reconciles_live_policy_without_rewriting_history() -> Result<()> {
    let session_id = "reconcile-queued-worker";
    let persisted = conflicting_reopen_state(session_id);
    let historical_frame_id = persisted.agent_frames[0].frame_node_id.clone();
    let store = SnapshotStore::with_state(persisted);
    let policy = lash_core::SessionPolicy {
        provider_id: "builder-provider".to_string(),
        model: model_spec("builder-model", None, 77_777),
        session_id: Some(session_id.to_string()),
        ..Default::default()
    };

    let state = crate::session::load_state_for_residency(
        lash_core::Residency::KeepAll,
        session_id,
        &policy,
        &store,
    )
    .await?;
    assert_eq!(state.policy.model, policy.model);
    assert_eq!(
        state
            .current_agent_frame()
            .expect("current frame")
            .assignment
            .policy
            .model
            .id,
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
            .model
            .id,
        "historical-model"
    );
    Ok(())
}

#[tokio::test]
async fn core_store_factory_is_used_for_managed_child_sessions() -> Result<()> {
    let factory = Arc::new(RecordingStoreFactory::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory.clone())
        .build()?;
    let session = core.session("root-with-child-store").open().await?;

    session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("managed-child-store".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "root-with-child-store".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await?;

    assert_eq!(
        factory.session_ids(),
        vec![
            "root-with-child-store".to_string(),
            "managed-child-store".to_string()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn reused_root_store_factory_reports_child_store_guidance() -> Result<()> {
    let reused_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(BoundSessionStore {
        session_id: "root-store".to_string(),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory {
            store: reused_store,
        }))
        .build()?;
    let session = core.session("root-store").open().await?;

    let err = session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("child-needs-own-store".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "root-store".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await
        .expect_err("reused root store should not open a child session");
    let message = err.to_string();

    assert!(message.contains("configured child session store is already bound"));
    assert!(message.contains("SessionBuilder::store"));
    assert!(message.contains("LashCoreBuilder::child_store_factory"));
    Ok(())
}

#[tokio::test]
async fn explicit_root_store_keeps_configured_child_store_factory() -> Result<()> {
    let factory = Arc::new(RecordingStoreFactory::default());
    let explicit_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory.clone())
        .build()?;
    let session = core
        .session("explicit-root-store")
        .store(explicit_store)
        .open()
        .await?;

    session
        .admin()
        .children()
        .create_session(SessionCreateRequest {
            session_id: Some("explicit-root-child".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "explicit-root-store".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await?;

    assert_eq!(
        factory.session_ids(),
        vec!["explicit-root-child".to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn explicit_session_store_takes_precedence_over_core_store_factory() -> Result<()> {
    let mut explicit_state = RuntimeSessionState {
        session_id: "store-precedence".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    explicit_state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::User,
        "explicit store",
    )]);
    let mut factory_state = explicit_state.clone();
    factory_state.append_active_conversation_messages(&[text_message(
        lash_core::MessageRole::Assistant,
        "factory store",
    )]);
    let explicit_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(SnapshotStore::with_state(explicit_state));
    let factory_store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(SnapshotStore::with_state(factory_state));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(ReusableStoreFactory {
            store: factory_store,
        }))
        .build()?;

    let reopened = core
        .session("store-precedence")
        .store(explicit_store)
        .open()
        .await?;
    let messages = reopened.read_view().messages().to_vec();

    assert_eq!(messages.len(), 1);
    assert_eq!(message_text(&messages[0]), "explicit store");
    Ok(())
}

#[test]
fn turn_result_total_usage_sums_parent_and_children() {
    use lash_core::{
        ExecutionSummary, OutputState, SessionPolicy, SessionSnapshot, TurnFinish, TurnOutcome,
    };

    let result = TurnResult {
        state: SessionSnapshot {
            session_id: "s".to_string(),
            policy: SessionPolicy::default(),
            ..Default::default()
        },
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "ok".to_string(),
        }),
        cancellation: None,
        assistant_output: AssistantOutput {
            safe_text: "ok".to_string(),
            raw_text: "ok".to_string(),
            state: OutputState::Usable,
        },
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        children_usage: vec![
            TokenLedgerEntry {
                source: "subagent".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
            },
            TokenLedgerEntry {
                source: "compaction".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
            },
        ],
        llm_calls: Vec::new(),
        tool_calls: Vec::new(),
        execution: ExecutionSummary::default(),
        errors: Vec::new(),
    };

    let total = result.total_usage();
    assert_eq!(total.input_tokens, 10 + 7 + 1);
    assert_eq!(total.output_tokens, 5 + 3);
    assert_eq!(total.cache_read_input_tokens, 2 + 4);
    assert_eq!(total.reasoning_output_tokens, 1);
    // Parent's own usage is unchanged.
    assert_eq!(result.usage.input_tokens, 10);
}

// =============================================================================
// Explicit runtime dependency wiring
// =============================================================================
//
/// An RLM builder with a model + provider already named, ready for the explicit
/// dependency wiring under test.
fn peer_coherence_builder(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
) -> crate::core::LashCoreBuilder {
    LashCore::rlm_builder(lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
}

/// The named in-memory Lashlang artifact store used by builder tests.
fn inline_artifact_store() -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
    Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new())
}

fn durable_session_store_factory(dir: &std::path::Path) -> Arc<dyn lash_core::SessionStoreFactory> {
    Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.join("sessions"),
    ))
}

fn durable_attachment_store(dir: &std::path::Path) -> Arc<dyn lash_core::AttachmentStore> {
    Arc::new(crate::persistence::FileAttachmentStore::new(
        dir.join("attachments"),
    ))
}

/// `LashCore` is not `Debug`, so `Result::expect_err` is unavailable; this
/// extracts the build error or panics with the given message.
fn expect_build_error<T>(result: std::result::Result<T, EmbedError>, message: &str) -> EmbedError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(err) => err,
    }
}

async fn durable_artifact_store(
    dir: &std::path::Path,
) -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
    Arc::new(
        lash_sqlite_store::Store::open(&dir.join("artifacts.db"))
            .await
            .expect("open durable artifact store"),
    )
}

async fn durable_process_env_store(
    dir: &std::path::Path,
) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
    Arc::new(
        lash_sqlite_store::Store::open(&dir.join("process-env.db"))
            .await
            .expect("open durable process env store"),
    )
}

async fn durable_trigger_store(dir: &std::path::Path) -> Arc<dyn lash_core::TriggerStore> {
    Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&dir.join("triggers.db"))
            .await
            .expect("open durable trigger store"),
    )
}

#[tokio::test]
async fn builder_requires_explicit_process_env_store_at_build() {
    let result = peer_coherence_builder(inline_artifact_store())
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash_core::InMemoryAttachmentStore::new()))
        .build();
    let err = expect_build_error(
        result,
        "builder must reject missing process execution environment store",
    );

    assert!(matches!(err, EmbedError::MissingProcessEnvStore));
}

#[tokio::test]
async fn all_durable_stores_build_successfully() -> Result<()> {
    // Positive control: a coherent durable wiring (durable session store +
    // durable attachment + durable artifact + durable process registry +
    // durable trigger store) builds without error.
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open durable registry"),
    );
    peer_coherence_builder(durable_artifact_store(dir.path()).await)
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build()?;
    Ok(())
}

#[tokio::test]
async fn durable_registry_with_only_child_store_factory_builds() -> Result<()> {
    // The CLI can wire a child store factory without a root store factory.
    // The process work runner must resolve the same effective factory.
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open durable registry"),
    );
    peer_coherence_builder(durable_artifact_store(dir.path()).await)
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .child_store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build()?;
    Ok(())
}

#[tokio::test]
async fn explicit_ephemeral_facets_build_successfully() -> Result<()> {
    // An all-in-memory build succeeds, including the explicit session store
    // factory that backs process execution.
    explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    Ok(())
}

struct NoopProcessRunHandle;

#[async_trait]
impl lash_core::ProcessRunHandle for NoopProcessRunHandle {
    async fn claim_and_run_pending(&self) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[tokio::test]
async fn process_work_driver_configures_external_runner_without_inline_store_factory() -> Result<()>
{
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let driver =
        lash_core::ProcessWorkDriver::new(Arc::clone(&registry), Arc::new(NoopProcessRunHandle));
    let driver_registry = driver.process_registry();
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .process_work_driver(driver)
        .build()?;

    let configured = core
        .process_registry()
        .expect("external driver configures the core registry");
    assert!(Arc::ptr_eq(&configured, &driver_registry));
    assert!(core.processes().observer().is_ok());
    assert!(core.work_driver.drivers().await.process.is_some());
    Ok(())
}

#[tokio::test]
async fn default_process_work_driver_resolves_when_registry_and_store_factory_present() -> Result<()>
{
    // Zero-ceremony path: a registry + a store factory (so the inline worker can
    // rebuild session runtimes) and no explicit driver constructs the default
    // inline process work driver on first `session().open()`. The driver's actual
    // lease-protected execution of out-of-turn processes is covered in lash-core
    // (`concurrent_workers_run_a_directly_registered_process_exactly_once`).
    let state = RuntimeSessionState {
        session_id: "main".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..Default::default()
        },
        ..Default::default()
    };
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    core.session("main").open().await?;
    assert!(
        core.work_driver.drivers().await.process.is_some(),
        "the default inline process driver must resolve when a registry + store factory are wired"
    );
    Ok(())
}

#[tokio::test]
async fn durable_process_worker_config_uses_core_process_registry() -> Result<()> {
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let trigger_store =
        Arc::new(lash_core::InMemoryTriggerStore::default()) as Arc<dyn lash_core::TriggerStore>;
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .trigger_store(Arc::clone(&trigger_store))
        .process_registry(Arc::clone(&registry))
        .build()?;

    assert!(core.processes().observer().is_ok());
    let config = core.durable_process_worker_config()?;
    let core_registry = core
        .process_registry()
        .expect("process registry must be configured");
    assert!(Arc::ptr_eq(&config.process_registry, &core_registry));
    assert!(Arc::ptr_eq(&config.trigger_store, &trigger_store));
    assert_eq!(
        config.process_execution_concurrency(),
        lash_core::DEFAULT_PROCESS_EXECUTION_CONCURRENCY
    );
    Ok(())
}

#[tokio::test]
async fn fork_inherits_process_grants_without_inheriting_wake_subscription() -> Result<()> {
    use lash_core::{ProcessRegistry as _, SessionStoreFactory as _};

    let factory = Arc::new(lash_core::InMemorySessionStoreFactory::new());
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build()?;
    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let policy = lash_core::SessionPolicy {
        provider_id: "fork-source-provider".to_string(),
        model: source_model,
        session_id: Some("fork-grant-source".to_string()),
        ..Default::default()
    };
    let source_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "fork-grant-source".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("create fork grant source");
    let source_incarnation_id = source_store
        .load_session_meta()
        .await
        .expect("load source metadata")
        .expect("source metadata exists")
        .incarnation_id;
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: "fork-grant-source".to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(source_incarnation_id),
        policy,
        ..Default::default()
    };
    source_state.ensure_agent_frame_initialized();
    source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state(
            &source_state,
            &[],
        ))
        .await
        .expect("commit fork grant source");
    let fork_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("fork grant source leaf");
    core.pin(&fork_node_id).await?;

    let source_scope = lash_core::SessionScope::new("fork-grant-source");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "fork-visible-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_wake_target(Some(source_scope.clone())),
        )
        .await
        .expect("register fork-visible process");
    registry
        .grant_handle(
            &source_scope,
            "fork-visible-process",
            lash_core::ProcessHandleDescriptor::new(Some("test"), Some("fork inherited")),
        )
        .await
        .expect("grant source process handle");

    core.fork_at(&fork_node_id, "fork-grant-branch").await?;
    let branch_store = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            session_id: "fork-grant-branch".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
        })
        .await
        .expect("open branch store")
        .expect("branch store exists");
    let branch_read = branch_store
        .load_session(lash_core::SessionReadScope::FullGraph)
        .await
        .expect("load branch config")
        .expect("branch head exists");
    assert_eq!(branch_read.config.provider_id, "fork-source-provider");
    assert_eq!(branch_read.config.model.id, "fork-source-model");

    let branch_scope = lash_core::SessionScope::new("fork-grant-branch");
    let inherited = registry
        .list_handle_grants(&branch_scope)
        .await
        .expect("list inherited grants");
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].0.process_id, "fork-visible-process");
    assert_eq!(
        inherited[0]
            .1
            .wake_target
            .as_ref()
            .map(|scope| scope.session_id.as_str()),
        Some("fork-grant-source"),
        "fork observes the process but does not become its wake target"
    );
    let published_meta = branch_store
        .load_session_meta()
        .await
        .expect("load published fork metadata")
        .expect("published fork metadata exists");
    let lash_core::SessionRelation::Fork { process_grants, .. } = &published_meta.relation else {
        panic!("branch metadata must retain its fork relation");
    };
    assert!(
        process_grants.is_empty(),
        "successfully published grants must be consumed from the recovery snapshot"
    );

    let mut recovery_meta = published_meta;
    let lash_core::SessionRelation::Fork { process_grants, .. } = &mut recovery_meta.relation
    else {
        unreachable!("relation was checked above");
    };
    process_grants.push(lash_core::ProcessHandleGrant {
        session_id: "fork-grant-source".to_string(),
        process_id: "fork-visible-process".to_string(),
        descriptor: lash_core::ProcessHandleDescriptor::new(Some("test"), Some("fork inherited")),
    });
    branch_store
        .save_session_meta(recovery_meta)
        .await
        .expect("simulate a crash before grant snapshot consumption");
    registry
        .revoke_handle(&branch_scope, "fork-visible-process")
        .await
        .expect("remove the partially published grant");
    core.session("fork-grant-branch").open().await?;
    assert_eq!(
        registry
            .list_handle_grants(&branch_scope)
            .await
            .expect("list recovered fork grants")
            .len(),
        1,
        "opening a durable fork must reconcile an unconsumed grant snapshot"
    );
    let recovered_meta = branch_store
        .load_session_meta()
        .await
        .expect("load recovered fork metadata")
        .expect("recovered fork metadata exists");
    let lash_core::SessionRelation::Fork { process_grants, .. } = &recovered_meta.relation else {
        unreachable!("relation was checked above");
    };
    assert!(
        process_grants.is_empty(),
        "recovery must consume the snapshot after idempotent publication"
    );
    registry
        .revoke_handle(&branch_scope, "fork-visible-process")
        .await
        .expect("deliberately revoke the recovered grant");
    core.session("fork-grant-branch").open().await?;
    assert!(
        registry
            .list_handle_grants(&branch_scope)
            .await
            .expect("list grants after deliberate revocation")
            .is_empty(),
        "a later deliberate revocation must remain revoked"
    );
    Ok(())
}

#[test]
fn builder_rejects_invalid_process_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
            .process_execution_concurrency(0)
            .build(),
        "zero process execution concurrency must be rejected",
    );
    assert!(matches!(err, EmbedError::ProcessExecutionConcurrency(_)));
}

#[tokio::test]
async fn durable_process_worker_config_requires_core_process_registry() {
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()
        .expect("build core without process support");

    let Err(err) = core.durable_process_worker_config() else {
        panic!("worker config must require process support");
    };
    assert!(matches!(err, EmbedError::MissingProcessRegistry));
}

#[tokio::test]
async fn registry_without_store_factory_fails_loudly() {
    // A registry but no store factory: the default work runner rebuilds a
    // session runtime per process and cannot do so without a store factory, so
    // build must fail loudly rather than silently leave processes unexecuted
    // (a process started in such a host would otherwise hang forever).
    let result = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build();
    let err = expect_build_error(
        result,
        "a process registry with no store factory must be rejected",
    );
    assert!(matches!(
        err,
        EmbedError::ProcessRegistryRequiresStoreFactory
    ));
}
