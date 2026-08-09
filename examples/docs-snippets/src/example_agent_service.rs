//! Compiled sources for the Rust snippets on `docs/example-agent-service.html`.

use lash::sync::MutexExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash::persistence::SessionStoreFactory;
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::tracing::{TraceLevel, TraceSink};
use lash::{LashCore, LashSession, TurnInput, TurnResult};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

async fn service_core(
    api_key: String,
    model: String,
    model_variant: String,
    data_dir: PathBuf,
    store_factory: Arc<dyn SessionStoreFactory>,
    trace_sink: Arc<dyn TraceSink>,
) -> anyhow::Result<()> {
    // docs:start:service-core
    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .into_components(),
    );
    let artifact_store = std::sync::Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("lash-artifacts.db")).await?,
    );
    let process_env_store = std::sync::Arc::new(
        lash_sqlite_store::Store::open(&data_dir.join("process-env.db")).await?,
    );
    let trigger_store = std::sync::Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db")).await?,
    );

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
        ),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .variant(lash::provider::ReasoningSelection::Effort(
                    model_variant.clone(),
                ))
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata")
                .with_capability(adaptive_reasoning_capability()),
        )
        .store_factory(store_factory)
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .process_env_store(process_env_store)
        .trigger_store(trigger_store)
        .attachment_store(std::sync::Arc::new(
            lash::persistence::FileAttachmentStore::new(data_dir.join("attachments")),
        ))
        .trace_sink(trace_sink)
        .trace_level(TraceLevel::Extended)
        .build()?;
    // docs:end:service-core
    Ok(())
}

struct AppState {
    core: LashCore,
}

impl AppState {
    async fn open_session(
        &self,
        chat_id: &str,
        model: lash::ModelSpec,
    ) -> anyhow::Result<LashSession> {
        Ok(self
            .core
            .session(chat_id)
            .session_spec(lash::SessionSpec::inherit().model(model))
            .open()
            .await?)
    }
}

struct ModelSelection {
    model: String,
    model_variant: Option<String>,
}

#[derive(Default)]
struct TurnUiState {
    assistant_prose: String,
}

fn assistant_text_for_persistence(_output: &TurnResult, _live_prose: &str) -> String {
    String::new()
}

async fn service_turn(
    state: &AppState,
    chat_id: String,
    text: String,
    model_selection: ModelSelection,
    ui_events: lash::runtime::NoopTurnActivitySink,
    turn_state: Arc<Mutex<TurnUiState>>,
) -> anyhow::Result<()> {
    // docs:start:service-turn
    let model = lash::ModelSpec::builder(model_selection.model)
        .variant(
            model_selection
                .model_variant
                .map(lash::provider::ReasoningSelection::Effort)
                .unwrap_or_default(),
        )
        .context_window_tokens(200_000)
        .build()
        .expect("valid model metadata")
        .with_capability(adaptive_reasoning_capability());
    let session = state.open_session(&chat_id, model).await?;
    let replay_cursor = session.observe().current_observation().cursor;

    use lash::rlm::RlmTurnBuilderExt as _;

    let turn = session.turn(TurnInput::text(text)).require_finish()?;

    let output = turn.stream_to(&ui_events).await?;
    let assistant_text =
        assistant_text_for_persistence(&output, &turn_state.lock_recover().assistant_prose);
    // docs:end:service-turn
    let _ = replay_cursor;
    Ok(())
}

fn adaptive_reasoning_capability() -> lash::provider::ModelCapability {
    lash::provider::ModelCapability {
        reasoning: Some(lash::provider::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            default_effort: Some("medium".to_string()),
            aliases: Default::default(),
            encoding: lash::provider::ReasoningEncoding::Effort,
            disable: None,
            mandatory: false,
        }),
        cache_control: Some(lash::provider::CacheControlDialect::Anthropic),
        stream_termination: None,
        sampling: lash::provider::SamplingCapability::Configurable,
    }
}
