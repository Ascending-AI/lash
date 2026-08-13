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
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
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
        .process_env_store(process_env_store)
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build()?;

    let session = core.session(chat_id).open().await?;
    let result = session
        .turn(TurnInput::text(user_text))
        .stream_to(&events)
        .await?;
    // docs:end:facade-turn
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_facade_builder_resolves() {
        let data_dir = tempfile::tempdir().expect("temporary docs-snippet directory");
        let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.path().join("sessions"),
        ));
        let result = facade_turn(
            crate::test_support::provider(),
            store_factory,
            Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::new()),
            data_dir.path().to_path_buf(),
            "docs-snippet-session",
            "hello".to_string(),
        )
        .await;

        crate::test_support::assert_builder_resolved(result);
    }
}
