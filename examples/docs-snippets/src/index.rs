//! Compiled source for the Rust snippets on `docs/index.html`.

// docs:start:hello-lash
use std::sync::Arc;

use lash::{LashCore, ModelSpec, PromptLayerSink, TurnInput, provider::ProviderHandle};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("OPENROUTER_API_KEY")?;
    let provider = ProviderHandle::new(
        OpenAiCompatibleProvider::new(api_key, OPENROUTER_BASE_URL)
            .with_compat(OpenAiCompat::openrouter())
            .into_components(),
    );

    let model = ModelSpec::builder("anthropic/claude-sonnet-4.6")
        .context_window_tokens(200_000)
        .capability(lash::provider::ModelCapability {
            cache_control: Some(lash::provider::CacheControlDialect::Anthropic),
            ..Default::default()
        })
        .build()?;

    // one LashCore per app, cloned freely.
    let core = LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(model)
        .instructions("Answer in one short sentence. Skip preamble.")
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .build()?;

    // one session per chat / task; run one turn; read settled prose.
    let session = core.session("hello-1").open().await?;
    let result = session
        .turn(TurnInput::text("Say hi in one short sentence."))
        .run()
        .await?;

    println!("{}", result.assistant_message().unwrap_or_default());
    Ok(())
}
// docs:end:hello-lash
