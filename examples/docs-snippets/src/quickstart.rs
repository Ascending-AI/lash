//! Compiled source for the Rust snippets on `docs/quickstart.html`.

// docs:start:hello-lash
use std::sync::Arc;

use lash::{LashCore, ModelSpec, PromptLayerSink, TurnInput, provider::ProviderHandle};
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // build a provider handle. substitute your own creds + base URL.
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

    let core = hello_lash_core(provider, model)?;

    // one session per chat / task.
    let session = core.session("hello-1").open().await?;

    // run one turn; read settled prose from the terminal result.
    let result = session
        .turn(TurnInput::text("Say hi in one short sentence."))
        .run()
        .await?;

    let prose = result.assistant_message().unwrap_or_default();
    println!("{prose}");
    Ok(())
}

// one LashCore per app, cloned freely.
fn hello_lash_core(provider: ProviderHandle, model: ModelSpec) -> lash::Result<LashCore> {
    LashCore::standard_builder(lash::TurnBudget::bounded(50))
        .provider(provider)
        .model(model)
        .instructions("Answer in one short sentence. Skip preamble.")
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build()
}
// docs:end:hello-lash

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_hello_lash_builder_resolves() {
        let model = ModelSpec::builder("docs-snippet-test")
            .context_window_tokens(4_096)
            .build()
            .expect("valid test model");

        hello_lash_core(ProviderHandle::unconfigured(), model)
            .expect("published hello-lash builder must resolve");
    }
}
