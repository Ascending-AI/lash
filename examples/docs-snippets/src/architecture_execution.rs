//! Compiled sources for the Rust snippets on `docs/architecture/execution.html`.

use std::path::PathBuf;
use std::sync::Arc;

use lash::TurnInput;
use lash::persistence::SessionStoreFactory;
use lash::provider::ProviderHandle;
use lash::runtime::NoopTurnActivitySink;

async fn facade_turn(
    provider: ProviderHandle,
    store_factory: Arc<dyn SessionStoreFactory>,
    data_dir: PathBuf,
    chat_id: &str,
    user_text: String,
) -> anyhow::Result<()> {
    let events = NoopTurnActivitySink;
    // docs:start:facade-turn
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("anthropic/claude-sonnet-4.6")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(std::sync::Arc::new(
            lash::durability::InlineEffectHost::default(),
        ))
        .attachment_store(std::sync::Arc::new(
            lash::persistence::FileAttachmentStore::new(data_dir.join("attachments")),
        ))
        .build()?;

    let session = core.session(chat_id).open().await?;
    let result = session
        .turn(TurnInput::text(user_text))
        .stream_to(&events)
        .await?;
    // docs:end:facade-turn
    Ok(())
}
