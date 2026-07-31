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

async fn lazy_projection() -> anyhow::Result<()> {
    let my_docs_projection = MyDocsProjection;
    // docs:start:lazy-projection
    use std::sync::Arc;

    use lash::rlm::{ProjectionRegistry, RlmProjectedBindings, RlmTurnInputExt};
    use lash::{TurnInput, plugins::runtime_plugin_stack};

    let registry = Arc::new(ProjectionRegistry::new());
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    )
    .with_projection_resolver(registry.clone());
    let core = lash::LashCore::rlm_builder(factory)
        .plugins(runtime_plugin_stack())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;

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

    let core = lash::LashCore::standard_builder()
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits("gpt-5.4", Default::default(), 200_000, None)
                .expect("valid model metadata"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .prompt_template(template)
        .prompt_contribution(PromptContribution::guidance(
            "App",
            "Answer as the host application assistant.",
        ))
        .build()?;

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

fn run_tone_tool(_name: &str, _args: &serde_json::Value, _tone: &str) -> lash::tools::ToolResult {
    lash::tools::ToolResult::ok(serde_json::Value::Null)
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
    ) -> Result<lash::tools::PreparedToolCall, lash::tools::ToolResult> {
        let Some(input) = call.context.plugin_input::<ToneTurnInput>(TonePlugin::ID) else {
            return Err(lash::tools::ToolResult::err_fmt("missing tone input"));
        };
        let prepared_payload = serde_json::to_value(input).map_err(|err| {
            lash::tools::ToolResult::err_fmt(format!("invalid tone input: {err}"))
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

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolResult {
        let input = match call.context.decode_prepared_payload::<ToneTurnInput>() {
            Ok(input) => input,
            Err(err) => {
                return lash::tools::ToolResult::err_fmt(format!("missing tone input: {err}"));
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
        lash::rlm::RlmProtocolPluginConfig::default(),
        std::sync::Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits(model.clone(), Default::default(), 200_000, None)
                .expect("valid model metadata"),
        )
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::InMemoryAttachmentStore::new(),
        ))
        .build()?;

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
