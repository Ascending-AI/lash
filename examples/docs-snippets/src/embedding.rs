//! Compiled sources for the Rust snippets on `docs/embedding.html`.

use std::sync::Arc;

use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
    runtime_plugin_stack,
};
use lash::provider::ProviderHandle;
use lash::tools::{ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolProvider, ToolResult};
use lash::{LashSession, SessionSpec, TurnActivity, TurnInput};

struct AppTools;

#[async_trait::async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
        ToolResult::ok(serde_json::Value::Null)
    }
}

struct AppPlugin;

impl SessionPlugin for AppPlugin {
    fn id(&self) -> &'static str {
        "app"
    }

    fn register(&self, _reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}

struct AppPluginFactory;

impl PluginFactory for AppPluginFactory {
    fn id(&self) -> &'static str {
        "app"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(AppPlugin))
    }
}

struct CustomBudgetPlugin;

impl PluginFactory for CustomBudgetPlugin {
    fn id(&self) -> &'static str {
        "tool_output_budget"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(AppPlugin))
    }
}

async fn full_core(provider: ProviderHandle) -> anyhow::Result<()> {
    // docs:start:full-core
    use std::sync::Arc;

    use lash::{TurnEvent, TurnInput, plugins::runtime_plugin_stack, tools::*};

    let data_dir = std::path::PathBuf::from(".lash-data");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        data_dir.join("sessions"),
    ));
    let artifact_store =
        Arc::new(lash_sqlite_store::Store::open(&data_dir.join("artifacts.db")).await?);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("anthropic/claude-sonnet-4.6")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .plugins(runtime_plugin_stack())
        .tools(Arc::new(AppTools) as Arc<dyn ToolProvider>)
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .build()?;

    let session = core.session("chat-123").open().await?;
    let result = session
        .turn(TurnInput::text("Use the app tools."))
        .run()
        .await?;
    let assistant_text: String = result
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    println!("{assistant_text}");
    // docs:end:full-core
    Ok(())
}

async fn preset_core(provider: ProviderHandle) -> anyhow::Result<()> {
    // docs:start:preset-core
    use std::sync::Arc;

    use lash::{SessionSpec, plugins::PluginFactory};

    let root_spec = SessionSpec::new().provider_id(provider.kind()).model(
        lash::ModelSpec::builder("gpt-5.4")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model metadata"),
    );

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .session_spec(root_spec)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .configure_plugins(|plugins| {
            plugins.push(Arc::new(AppPluginFactory) as Arc<dyn PluginFactory>);
        })
        .build()?;
    // docs:end:preset-core
    let _ = core;
    Ok(())
}

async fn custom_stack(root_spec: SessionSpec) -> anyhow::Result<()> {
    // docs:start:custom-stack
    use std::sync::Arc;

    let plugins = runtime_plugin_stack().configure(|plugins| {
        plugins.replace(Arc::new(CustomBudgetPlugin) as Arc<dyn PluginFactory>);
        plugins.push(Arc::new(AppPluginFactory) as Arc<dyn PluginFactory>);
    });

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .session_spec(root_spec)
        .plugins(plugins)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;
    // docs:end:custom-stack
    let _ = core;
    Ok(())
}

async fn collected_turn(session: &LashSession) -> anyhow::Result<()> {
    use lash::TurnEvent;
    // docs:start:collected-turn
    let collected = session
        .turn(TurnInput::text("Summarize this task."))
        .run()
        .await?;

    let live_preview: String = collected
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    let settled_answer = collected.assistant_message().unwrap_or_default();
    let total = collected.result.total_usage(); // parent + children
    let parent_usage = collected.result.usage; // parent's own LLM tokens
    let children = collected.result.children_usage; // per-(source, model) child entries
    let outcome = collected.result.outcome;
    // docs:end:collected-turn
    Ok(())
}

struct AppEvents {
    tx: tokio::sync::mpsc::Sender<TurnActivity>,
}

impl AppEvents {
    fn new(tx: tokio::sync::mpsc::Sender<TurnActivity>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for AppEvents {
    async fn emit(&self, activity: TurnActivity) {
        let _ = self.tx.send(activity).await;
    }
}

fn persist(_assistant_text: &str, _usage: lash::usage::TokenUsage) -> anyhow::Result<()> {
    Ok(())
}

async fn streamed_turn(
    session: &LashSession,
    user_text: String,
    tx: tokio::sync::mpsc::Sender<TurnActivity>,
) -> anyhow::Result<()> {
    // docs:start:streamed-turn
    let ui_sink = Arc::new(AppEvents::new(tx));
    let turn = session
        .turn(TurnInput::text(user_text))
        .stream_to(ui_sink.as_ref())
        .await?;

    persist(
        turn.assistant_message().unwrap_or_default(),
        turn.total_usage(),
    )?;
    // docs:end:streamed-turn
    Ok(())
}

fn render_pending_inputs(_inputs: &[lash::PendingTurnInput]) {}

fn remove_pending_input_preview(_input_id: &str) {}

fn reconcile_pending_input_state(_input_id: Option<&str>) {}

async fn pending_input_reconciliation(session: &LashSession) -> anyhow::Result<()> {
    // docs:start:pending-input-reconciliation
    use lash::{
        PendingTurnInputCancelOutcome, PendingTurnInputCancelTarget,
        PendingTurnInputSuffixCancelOutcome, TurnInput,
    };

    session
        .enqueue(TurnInput::text("first draft"))
        .id("message:1")
        .send()
        .await?;
    let second = session
        .enqueue(TurnInput::text("second draft"))
        .id("message:2")
        .send()
        .await?;

    // Queue previews come from runtime reconciliation, not local draft state.
    // Persist the acceptance receipt's `input_id` or `source_key` beside the
    // product message.
    let pending = session.pending_turn_inputs().await?;
    render_pending_inputs(&pending);

    // Before editing a product message, atomically cancel the runtime suffix
    // rooted at that submitted revision.
    let anchor = second
        .source_key
        .clone()
        .map(PendingTurnInputCancelTarget::source_key)
        .unwrap_or_else(|| PendingTurnInputCancelTarget::input_id(second.input_id.clone()));
    match session.cancel_pending_turn_input_suffix(anchor).await? {
        PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. } => {
            render_pending_inputs(&session.pending_turn_inputs().await?);
        }
        PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } => {
            for outcome in outcomes {
                match outcome {
                    PendingTurnInputCancelOutcome::Cancelled(input)
                    | PendingTurnInputCancelOutcome::AlreadyCancelled(input) => {
                        remove_pending_input_preview(&input.input_id);
                    }
                    PendingTurnInputCancelOutcome::AlreadyClaimed { input, .. }
                    | PendingTurnInputCancelOutcome::AlreadyCompleted(input) => {
                        reconcile_pending_input_state(Some(&input.input_id));
                    }
                    PendingTurnInputCancelOutcome::NotFound => {
                        reconcile_pending_input_state(None);
                    }
                }
            }
        }
    }

    let replacement = session
        .enqueue(TurnInput::text("updated second draft"))
        .id("message:2:v2")
        .send()
        .await?;
    assert_eq!(replacement.source_key.as_deref(), Some("host:message:2:v2"));
    render_pending_inputs(&session.pending_turn_inputs().await?);
    // docs:end:pending-input-reconciliation
    Ok(())
}
