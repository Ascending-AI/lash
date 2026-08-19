//! Compiled sources for the Rust snippets on `docs/embedding-prompts.html`.

use std::sync::Arc;

use lash::PluginBinding;
use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash::provider::ProviderHandle;
use lash::{LashSession, TurnInput};

#[derive(serde::Serialize)]
struct Task {
    name: String,
}

#[derive(serde::Serialize)]
struct Board {
    cells: Vec<u8>,
}

async fn projected_bindings(session: &LashSession, task: Task, board: Board) -> anyhow::Result<()> {
    // docs:start:projected-bindings
    use lash::TurnInput;
    use lash::rlm::{RlmProjectedBindings, RlmTurnInputExt, rlm_session_projection_extension};

    // Session-wide: applies to every turn the session runs.
    session
        .admin()
        .protocol()
        .apply_session_extension(rlm_session_projection_extension(
            RlmProjectedBindings::new()
                .bind_json("tenant_id", serde_json::json!("acme"))?
                .bind_json("task", serde_json::to_value(&task)?)?,
        ))
        .await?;

    // Per-turn: layered on top of the session bindings for this turn only.
    let input = TurnInput::text("Play one move.").rlm_project(
        RlmProjectedBindings::new().bind_json("board", serde_json::to_value(&board)?)?,
    )?;

    let result = session.turn(input).run().await?;
    // docs:end:projected-bindings
    Ok(())
}

struct MyDocsProjection;

impl lashlang::ProjectedHostDescriptor for MyDocsProjection {
    fn type_name(&self) -> &str {
        "Docs"
    }
}

async fn lazy_projection(provider: ProviderHandle, model: lash::ModelSpec) -> anyhow::Result<()> {
    let my_docs_projection = MyDocsProjection;
    // docs:start:lazy-projection
    use std::sync::Arc;

    use lash::rlm::{ProjectionRegistry, RlmProjectedBindings, RlmTurnInputExt};
    use lash::{TurnInput, plugins::runtime_plugin_stack};

    let registry = Arc::new(ProjectionRegistry::new());
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    )
    .with_projection_resolver(registry.clone());
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .plugins(runtime_plugin_stack())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    // `my_docs_projection` implements `lashlang::ProjectedHostDescriptor`.
    let docs_ref = registry.register_memory(Arc::new(my_docs_projection));
    let input = TurnInput::text("Answer using docs only when needed.")
        .rlm_project(RlmProjectedBindings::new().bind_lazy("docs", docs_ref)?)?;
    // docs:end:lazy-projection
    Ok(())
}

async fn prompt_template(provider: ProviderHandle) -> anyhow::Result<()> {
    // docs:start:prompt-template
    use std::sync::Arc;

    use lash::prompt::{
        PromptBuiltin, PromptContribution, PromptSlot, PromptTemplate, PromptTemplateEntry,
        PromptTemplateSection,
    };
    use lash::{PromptLayerSink, TurnInput};

    let template = PromptTemplate::new(vec![
        PromptTemplateSection::untitled(vec![
            PromptTemplateEntry::builtin(PromptBuiltin::MainAgentIntro),
            PromptTemplateEntry::slot(PromptSlot::Intro),
        ]),
        PromptTemplateSection::titled(
            "Guidance",
            vec![PromptTemplateEntry::slot(PromptSlot::Guidance)],
        ),
    ]);

    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("gpt-5.4")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .prompt_template(template)
        .prompt_contribution(PromptContribution::guidance(
            "App",
            "Answer as the host application assistant.",
        ))
        .build(crate::example_process_owner())?;

    let session = core
        .session("customer-42")
        .replace_prompt_slot(
            PromptSlot::Guidance,
            [PromptContribution::guidance(
                "Tenant",
                "Use the tenant's support policy.",
            )],
        )
        .open()
        .await?;

    let result = session
        .turn(TurnInput::text("Draft the response."))
        .prompt_contribution(PromptContribution::guidance(
            "Turn",
            "Keep this reply under 120 words.",
        ))
        .run()
        .await?;
    // docs:end:prompt-template
    Ok(())
}

struct TonePluginFactory;

impl PluginFactory for TonePluginFactory {
    fn id(&self) -> &'static str {
        TonePlugin::ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(ToneSessionPlugin))
    }
}

struct ToneSessionPlugin;

struct ToneTools;

fn tone_tool_definitions() -> Vec<lash::tools::ToolDefinition> {
    Vec::new()
}

fn run_tone_tool(_name: &str, _args: &serde_json::Value, _tone: &str) -> lash::tools::ToolOutcome {
    lash::tools::ToolOutcome::ok(serde_json::Value::Null)
}

// docs:start:tone-plugin
#[derive(Clone, Debug)]
struct ToneConfig;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ToneTurnInput {
    tone: String,
}

#[derive(Clone, Debug)]
struct TonePlugin;

impl lash::PluginBinding for TonePlugin {
    const ID: &'static str = "tone";
    type SessionConfig = ToneConfig;
    type Input = ToneTurnInput;

    fn factory(_: &Self::SessionConfig) -> Arc<dyn lash::plugins::PluginFactory> {
        Arc::new(TonePluginFactory)
    }

    fn requires_turn_input(_: &Self::SessionConfig) -> bool {
        true
    }
}

impl lash::plugins::SessionPlugin for ToneSessionPlugin {
    fn id(&self) -> &'static str {
        TonePlugin::ID
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        reg.prompt().contribute(Arc::new(|ctx| {
            Box::pin(async move {
                let Some(input) = ctx
                    .turn_context
                    .plugin_input::<ToneTurnInput>(TonePlugin::ID)
                else {
                    return Ok(Vec::new());
                };
                Ok(vec![lash::prompt::PromptContribution::environment(
                    "Tone",
                    format!("Use this response tone: {}", input.tone),
                )])
            })
        }));
        reg.tools().provider(Arc::new(ToneTools))
    }
}

#[async_trait::async_trait]
impl lash::tools::ToolProvider for ToneTools {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        tone_tool_definitions()
            .into_iter()
            .map(|definition| definition.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        tone_tool_definitions()
            .into_iter()
            .find(|definition| definition.name() == name)
            .map(|definition| Arc::new(definition.contract()))
    }

    // Typed turn input is read at prepare time, where ToolPrepareContext
    // exposes plugin_input, then threaded into execute as the prepared payload.
    async fn prepare_tool_call(
        &self,
        call: lash::tools::ToolPrepareCall<'_>,
    ) -> Result<lash::tools::PreparedToolCall, lash::tools::ToolOutcome> {
        let Some(input) = call.context.plugin_input::<ToneTurnInput>(TonePlugin::ID) else {
            return Err(lash::tools::ToolOutcome::err_fmt("missing tone input"));
        };
        let prepared_payload = serde_json::to_value(input).map_err(|err| {
            lash::tools::ToolOutcome::err_fmt(format!("invalid tone input: {err}"))
        })?;
        Ok(lash::tools::PreparedToolCall::from_parts(
            call.pending.call_id,
            call.tool_id.clone(),
            call.pending.tool_name,
            call.pending.args,
            call.pending.replay,
            prepared_payload,
        ))
    }

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolOutcome {
        let input = match call.context.decode_prepared_payload::<ToneTurnInput>() {
            Ok(input) => input,
            Err(err) => {
                return lash::tools::ToolOutcome::err_fmt(format!("missing tone input: {err}"));
            }
        };
        run_tone_tool(call.name, call.args, &input.tone)
    }
}
// docs:end:tone-plugin

// docs:start:tone-turn-ext
trait ToneTurnExt {
    fn with_tone(self, tone: impl Into<String>) -> Self;
}

impl ToneTurnExt for lash::TurnBuilder {
    fn with_tone(self, tone: impl Into<String>) -> Self {
        self.with_plugin_input::<TonePlugin>(ToneTurnInput { tone: tone.into() })
    }
}
// docs:end:tone-turn-ext

async fn tone_session(
    provider: ProviderHandle,
    model: String,
    chat_id: &str,
    sink: lash::runtime::NoopTurnActivitySink,
) -> anyhow::Result<()> {
    // docs:start:tone-session
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        std::sync::Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::InMemoryAttachmentStore::new(),
        ))
        .process_env_store(std::sync::Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    let session = core
        .session(chat_id)
        .plugin::<TonePlugin>(ToneConfig)
        .open()
        .await?;

    use lash::rlm::RlmTurnBuilderExt as _;

    let result = session
        .turn(TurnInput::text("Summarize this incident."))
        .with_tone("brief and factual")
        .require_finish()?
        .stream_to(&sink)
        .await?;
    // docs:end:tone-session
    Ok(())
}

#[cfg(test)]
mod asserted_examples {
    use std::collections::HashMap;

    use lash::prompt::{
        PromptBuiltin, PromptContribution, PromptLayer, PromptSlot, PromptTemplate,
        PromptTemplateEntry, PromptTemplateSection, default_prompt_template,
    };
    use lash::remote::prompt::{
        RemotePromptBuiltin, RemotePromptContribution, RemotePromptContributionGate,
        RemotePromptLayer, RemotePromptSlot, RemotePromptSlotLayer, RemotePromptTemplate,
        RemotePromptTemplateEntry, RemotePromptTemplateSection,
    };

    #[test]
    fn prompt_layers_preserve_host_overrides_across_the_remote_boundary() {
        let empty = PromptLayer::new();
        assert!(PromptLayer::is_empty(&empty));

        let template = PromptTemplate::new(vec![
            PromptTemplateSection::untitled(vec![
                PromptTemplateEntry::builtin(PromptBuiltin::MainAgentIntro),
                PromptTemplateEntry::slot(PromptSlot::Intro),
                PromptTemplateEntry::Text {
                    content: "Host preamble".to_string(),
                },
                PromptTemplateEntry::text("Host contract"),
            ]),
            PromptTemplateSection::titled(
                "Execution",
                vec![
                    PromptTemplateEntry::Builtin {
                        builtin: PromptBuiltin::ExecutionInstructions,
                    },
                    PromptTemplateEntry::Slot {
                        slot: PromptSlot::Execution,
                    },
                ],
            ),
            PromptTemplateSection::new(
                Some("Guidance".to_string()),
                vec![
                    PromptTemplateEntry::Builtin {
                        builtin: PromptBuiltin::CoreGuidance,
                    },
                    PromptTemplateEntry::Slot {
                        slot: PromptSlot::ProjectInstructions,
                    },
                    PromptTemplateEntry::slot(PromptSlot::Guidance),
                    PromptTemplateEntry::slot(PromptSlot::RuntimeContext),
                    PromptTemplateEntry::slot(PromptSlot::Environment),
                ],
            ),
        ]);
        assert_eq!(template.sections.len(), 3);
        assert_eq!(template.sections[1].title.as_deref(), Some("Execution"));
        assert_eq!(template.sections[2].entries.len(), 5);

        let gated = PromptContribution::new(
            PromptSlot::Guidance,
            "Repository policy",
            "Use the workspace formatter.",
        )
        .with_priority(-20)
        .requires_any_tool(["read_file", "write_file"]);
        assert_eq!(gated.slot, PromptSlot::Guidance);
        assert_eq!(gated.title.as_deref(), Some("Repository policy"));
        assert_eq!(gated.priority, -20);
        assert_eq!(gated.content.as_ref(), "Use the workspace formatter.");
        assert!(!gated.gate.is_empty());
        assert_eq!(gated.gate.tools, ["read_file", "write_file"]);
        let single_gate =
            PromptContribution::guidance("Safety", "Ask before publishing.").requires_tool("ask");
        assert_eq!(single_gate.gate.tools, ["ask"]);

        let mut layer = PromptLayer::with_template(template.clone());
        assert_eq!(layer.template.as_ref(), Some(&template));
        assert!(layer.slots.is_empty());
        layer.add_contribution(PromptContribution::intro(
            "Host",
            "Welcome to the workbench.",
        ));
        layer.add_contribution(PromptContribution::execution(
            "Runbook",
            "Validate before reporting completion.",
        ));
        layer.add_contribution(gated);
        layer.add_contribution(PromptContribution::project_instructions(
            "Keep public contracts stable.",
        ));
        layer.add_contribution(PromptContribution::runtime_context(
            "PostgreSQL is available on the test port.",
        ));
        layer.add_contribution(PromptContribution::environment(
            "Workspace",
            "/workspace/code/lash",
        ));
        layer = layer.with_contribution(single_gate);
        layer.replace_slot(
            PromptSlot::Environment,
            [PromptContribution::environment(
                "Workspace",
                "/workspace/code/lash-figex2a",
            )],
        );
        layer.clear_slot(PromptSlot::RuntimeContext);

        let configured = PromptLayer::new()
            .prompt_template(template.clone())
            .with_replaced_slot(
                PromptSlot::Guidance,
                [PromptContribution::guidance(
                    "Override",
                    "Prefer exact evidence.",
                )],
            )
            .with_cleared_slot(PromptSlot::ProjectInstructions);
        assert_eq!(configured.template.as_ref(), Some(&template));
        assert!(configured.slots[&PromptSlot::Guidance].reset);
        assert!(configured.slots[&PromptSlot::ProjectInstructions].reset);
        assert!(
            configured.slots[&PromptSlot::ProjectInstructions]
                .contributions
                .is_empty()
        );
        assert!(
            PromptLayer::with_template(template.clone())
                .clear_template()
                .template
                .is_none()
        );

        let remote: RemotePromptLayer = layer.clone().into();
        assert!(!RemotePromptLayer::is_empty(&remote));
        assert!(remote.slots[&RemotePromptSlot::Environment].reset);
        assert_eq!(
            remote.slots[&RemotePromptSlot::Environment].contributions[0].content,
            "/workspace/code/lash-figex2a"
        );
        assert!(
            remote.slots[&RemotePromptSlot::RuntimeContext]
                .contributions
                .is_empty()
        );
        let round_trip: PromptLayer = remote.clone().into();
        assert_eq!(round_trip, layer);

        let remote_template = RemotePromptTemplate {
            sections: vec![RemotePromptTemplateSection {
                title: Some("Remote policy".to_string()),
                entries: vec![
                    RemotePromptTemplateEntry::Text {
                        content: "Remote preamble".to_string(),
                    },
                    RemotePromptTemplateEntry::Builtin {
                        builtin: RemotePromptBuiltin::MainAgentIntro,
                    },
                    RemotePromptTemplateEntry::Builtin {
                        builtin: RemotePromptBuiltin::ExecutionInstructions,
                    },
                    RemotePromptTemplateEntry::Builtin {
                        builtin: RemotePromptBuiltin::CoreGuidance,
                    },
                    RemotePromptTemplateEntry::Slot {
                        slot: RemotePromptSlot::Guidance,
                    },
                ],
            }],
        };
        let remote_layer = RemotePromptLayer {
            template: Some(remote_template),
            slots: HashMap::from([
                (
                    RemotePromptSlot::Intro,
                    RemotePromptSlotLayer {
                        reset: false,
                        contributions: vec![RemotePromptContribution {
                            slot: RemotePromptSlot::Intro,
                            title: Some("Remote host".to_string()),
                            priority: -10,
                            gate: RemotePromptContributionGate {
                                tools: vec!["remote_search".to_string()],
                            },
                            content: "Use the remote tool registry.".to_string(),
                        }],
                    },
                ),
                (
                    RemotePromptSlot::Execution,
                    RemotePromptSlotLayer::default(),
                ),
                (
                    RemotePromptSlot::ProjectInstructions,
                    RemotePromptSlotLayer::default(),
                ),
                (
                    RemotePromptSlot::RuntimeContext,
                    RemotePromptSlotLayer::default(),
                ),
                (
                    RemotePromptSlot::Environment,
                    RemotePromptSlotLayer::default(),
                ),
            ]),
        };
        assert!(
            !remote_layer.slots[&RemotePromptSlot::Intro].contributions[0]
                .gate
                .is_empty()
        );
        let wire = serde_json::to_value(&remote_layer).expect("remote prompt layer must serialize");
        assert_eq!(wire["template"]["sections"][0]["title"], "Remote policy");
        assert_eq!(
            wire["slots"]["intro"]["contributions"][0]["gate"]["tools"][0],
            "remote_search"
        );
        assert_eq!(RemotePromptLayer::new(), RemotePromptLayer::default());

        let default_template = default_prompt_template();
        assert_eq!(default_template.sections.len(), 4);
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[tokio::test]
    async fn documented_prompt_builders_resolve() {
        lazy_projection(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("lazy-projection snippet must build");

        crate::test_support::assert_builder_resolved(
            prompt_template(crate::test_support::provider()).await,
        );
        crate::test_support::assert_builder_resolved(
            tone_session(
                crate::test_support::provider(),
                "docs-snippet-test".to_string(),
                "docs-snippet-session",
                lash::runtime::NoopTurnActivitySink,
            )
            .await,
        );
    }
}
