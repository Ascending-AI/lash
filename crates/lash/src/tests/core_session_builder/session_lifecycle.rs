// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
#[cfg(feature = "rlm")]
use crate::rlm::RlmSendBuilderExt as _;
#[path = "session_lifecycle/commit_budget.rs"]
mod commit_budget;

#[path = "session_lifecycle/session_binding.rs"]
mod session_binding;

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

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
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
        reg.context().prepare_turn(0, Arc::clone(&self.transform))?;
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
        model: Some(recorded_llm_profile(llm_profile_spec(
            "historical-model",
            None,
            11_111,
        ))),
        ..lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
    };
    let current_policy = lash_core::SessionPolicy {
        model: Some(recorded_llm_profile(llm_profile_spec(
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
        node_id: lash_core::NodeId::fixture(frame_node_id.to_string()),
        parent_node_id: state.session_graph.leaf_node_id.clone(),
        timestamp: "2026-07-27T00:00:00.000000000Z"
            .parse()
            .unwrap_or_else(|error| panic!("invalid fixture timestamp: {error}")),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core::AgentFrameReason::continue_as(),
            assignment: lash_core::AgentFrameAssignment::unconfigured(current_policy),
        },
    }));
    state.session_graph = lash_core::SessionGraph::from_shared_nodes(
        nodes,
        Some(lash_core::NodeId::fixture(frame_node_id.to_string())),
    )
    .expect("session lifecycle fixture graph is valid");
    state.current_frame_node_id = Some(frame_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(session_id);
    state.policy = lash_core::SessionPolicy {
        model: Some(recorded_llm_profile(llm_profile_spec(
            "top-level-model",
            None,
            33_333,
        ))),
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

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
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
        )
        .expect("valid declared tool schemas"),
        name.to_string(),
    )
}

/// The standard prompt is recorded config (FIG-4589): each session records
/// the prompt its own spec states, the host's default spec for one and
/// another spec for the other, and a prompt command replaces it for the
/// runs after it.
#[tokio::test]
async fn the_standard_prompt_comes_from_the_session_spec_and_its_command() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(
            recording_prompt_provider(Arc::clone(&seen)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let host_default = mock_session_spec()
        .plugin(
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
        .map_err(EmbedError::ProtocolTurnOptions)?;

    let defaulted = core
        .session("prompt-core-default")
        .created_with(host_default)
        .await
        .open()
        .await?;
    defaulted.send(TurnInput::text("first")).output().await?;

    core.session("prompt-own")
        .create(crate::SessionCreation {
            spec: mock_session_spec()
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
            parent: None,
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
        "the host default spec's intro is stated once: {}",
        prompts[0]
    );
    assert!(prompts[0].contains("Core instruction."));
    assert!(prompts[1].contains("Session intro."));
    assert!(prompts[1].contains("Session instruction."));
    assert!(
        !prompts[1].contains("Core"),
        "a session records only the prompt its own spec states: {}",
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
    let models = test_catalog(
        core_provider,
        [llm_profile_spec("core-model", None, 200_000)],
    );
    let registry = Arc::try_unwrap(models)
        .unwrap_or_else(|_| unreachable!("a fresh catalog has one owner"))
        .register(
            "session-model",
            lash_core::RegisteredLlmProfile::new(
                llm_profile_spec("session-model", None, 200_000),
                session_provider,
            ),
        )
        .expect("a second key registers");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .llm_profiles(Arc::new(registry))
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
    core.session("main")
        .create(crate::SessionCreation::root(crate::SessionSpec::new(
            "session-model",
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        )))
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
            crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("updated-model"),
            }),
        )
        .await?;
    let crate::config::ConfigTransactionOutcome::Refused { refusal } = refused else {
        panic!("a key the host does not serve is refused: {refused:?}");
    };
    assert_eq!(refusal.owner, crate::config::CORE_CONFIG_OWNER);
    assert_eq!(
        refusal.owner_refusal::<crate::config::CoreConfigRefusal>(),
        Some(crate::config::CoreConfigRefusal::UnknownLlmProfile {
            key: lash_core::LlmProfileKey::new("updated-model"),
        }),
        "the refusal is the core owner's typed unknown-model refusal: {refusal:?}"
    );

    let after_refusal = session.send(TurnInput::text("hello")).output().await?;
    assert_eq!(assistant_prose(&after_refusal.activities), "session");
    Ok(())
}

/// The reasoning a session's spec states is recorded with the session's
/// model and reaches the request unchanged.
#[tokio::test]
async fn the_spec_reasoning_is_recorded_and_reaches_the_request() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(
            recording_text_provider(
                "core-provider",
                "core-model",
                Some("core-variant"),
                "core",
                Arc::clone(&seen),
            ),
            llm_profile_spec("core-model", Some("core-variant".to_string()), 200_000),
        )
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
    let spec = session_spec_for(&llm_profile_spec(
        "core-model",
        Some("core-variant".to_string()),
        200_000,
    ))
    .reasoning(lash_core::ReasoningSelection::Effort(
        "core-variant".to_string(),
    ));
    let session = core.session("main").created_with(spec).await.open().await?;
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
    let core = LashCore::rlm_builder(backend, factory)
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
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
        .await
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
        .serve_test_llm_profile(
            recording_request_provider(Arc::clone(&seen)),
            mock_llm_profile_spec(),
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
            spec: mock_session_spec().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                        ..lash_rlm_types::RlmCreateExtras::default()
                    },
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
            parent: None,
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
        .serve_test_llm_profile(
            recording_request_provider(Arc::clone(&seen)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;

    core.session("rlm-format-survives-reopen")
        .create(crate::SessionCreation {
            spec: mock_session_spec().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash_rlm_types::RlmCreateExtras {
                        final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
                        ..lash_rlm_types::RlmCreateExtras::default()
                    },
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
            parent: None,
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
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let _parent = core.session("rlm-root").created().await.open().await?;
    let mut plugin_options = lash_core::PluginOptions {
        plugins: BTreeMap::new(),
    };
    plugin_options.insert_versioned(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
        crate::plugins::FormatVersion::ONE,
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
            spec: mock_session_spec().plugin_options(plugin_options),
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
    let backend = double_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).created().await.open().await?;
    materialize_session(&session).await?;
    let mut state = session.admin().state().persist_current().await?;
    state.set_execution_state_snapshot(Some(old_version_snapshot.into()));
    let store = lash_core::runtime::live_session_view(
        &backend.session_store_factory(),
        &SessionId::from(session_id),
    )
    .await?
    .expect("the admitted session");
    store
        .commit_runtime_state(
            lash_core::RuntimeCommit::persisted_state_with_operation_for_testing(
                &state,
                seed_operation("old-execution-snapshot"),
            ),
        )
        .await?;
    drop(session);

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
async fn park_then_resume_preserves_session_transcript() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
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
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("busy").created().await.open().await?;
    // A live clone shares the underlying runtime handle, exactly as an in-flight
    // turn would: parking must refuse rather than silently flush a session that
    // something else is still executing.
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
async fn public_session_state_appends_preserve_concurrent_retirement_refusals() -> Result<()> {
    let backend = double_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
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

#[cfg(feature = "rlm")]
#[tokio::test]
/// FIG-4099: a model change is a config patch, and the patched model reaches
/// every runtime consumer. (It used to be a reopen that stated the model.)
async fn a_native_model_patch_reaches_all_runtime_consumers() -> Result<()> {
    use lash_subagents::Capability as _;

    let session_id = "reconcile-open";
    let builder_model = llm_profile_spec("builder-model", None, 77_777);
    let historical_model = llm_profile_spec("historical-model", None, 11_111);
    let current_frame_model = llm_profile_spec("current-frame-model", None, 22_222);
    let top_level_model = llm_profile_spec("top-level-model", None, 33_333);
    let backend = double_backend().await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_probe = Arc::clone(&requests);
    let response_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let request_probe = Arc::clone(&request_probe);
            let response_counter = Arc::clone(&response_counter);
            async move {
                request_probe
                    .lock_recover()
                    .push(request.model.wire_model().to_string());
                let response_index = response_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if response_index == 0 {
                    Ok(text_response(&typescript_block(
                        r#"finish("historical");"#,
                    )))
                } else if response_index == 1 || response_index == 3 {
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
        .llm_profiles(test_catalog(
            provider,
            [
                builder_model.clone(),
                historical_model.clone(),
                current_frame_model.clone(),
                top_level_model.clone(),
            ],
        ))
        .plugin(probe_factory)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&historical_model))
        .await
        .open()
        .await?;
    assert_eq!(
        session.policy_snapshot().wire_model(),
        Some("historical-model"),
        "creation records the historical model"
    );
    let historical = session
        .send(TurnInput::text("record the historical frame"))
        .output()
        .await?;
    assert!(historical.is_success(), "{historical:?}");
    let historical_frame_id = session
        .admin()
        .state()
        .persist_current()
        .await?
        .current_frame_node_id
        .expect("the admitted historical turn owns a frame");
    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("current-frame-model"),
            },
        ))
        .await?;
    let current_frame = session
        .send(TurnInput::text("open the current frame"))
        .output()
        .await?;
    assert!(current_frame.is_success(), "{current_frame:?}");
    assert_eq!(
        session
            .admin()
            .state()
            .persist_current()
            .await?
            .current_agent_frame()
            .expect("the continued turn owns its current frame")
            .assignment
            .policy
            .wire_model(),
        Some("current-frame-model")
    );
    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("top-level-model"),
            },
        ))
        .await?;
    drop(session);
    let session = core
        .session(session_id)
        .created_with(session_spec_for(&top_level_model))
        .await
        .open()
        .await?;
    assert_eq!(
        session.policy_snapshot().wire_model(),
        Some("top-level-model"),
        "the reopen runs the recorded model"
    );
    requests.lock_recover().clear();
    transform_observations.lock_recover().clear();

    session
        .admin()
        .config()
        .configure(crate::config::ConfigTransaction::of(
            crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("builder-model"),
            },
        ))
        .await?;

    let policy = session.policy_snapshot();
    assert_eq!(
        policy.model,
        Some(recorded_llm_profile(builder_model.clone()))
    );
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
        Some(recorded_llm_profile(builder_model.clone()))
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
            fleet_format: lash_core::FleetFormat::current(),
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
        Some(recorded_llm_profile(builder_model.clone()))
    );
    println!(
        "consumer 5 child tier inheritance: model={} context_window_tokens={}",
        child_policy.wire_model().unwrap_or_default(),
        child_policy.context_window_tokens().unwrap_or_default()
    );

    let execution_env = state.process_execution_env_spec(&policy);
    assert_eq!(
        execution_env.policy.model,
        Some(recorded_llm_profile(builder_model.clone()))
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
    let builder_model = llm_profile_spec("builder-model", None, 77_777);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(mock_provider(), builder_model.clone())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session(session_id)
        .created_with(session_spec_for(&builder_model))
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
    // The supplied state is the session's: its model survives, whatever the
    // session was created with.
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
