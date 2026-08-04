//! Compiled sources for the Rust snippets on `docs/persistence.html`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use lash::provider::ProviderHandle;
use lash::{LashCore, LashSession, TurnInput, TurnOutput};
use lash_sqlite_store::SqliteSessionStoreFactory;

async fn sqlite_core(provider: ProviderHandle, model: String) -> anyhow::Result<()> {
    // docs:start:sqlite-core
    use std::sync::Arc;

    use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

    let data_dir = std::path::PathBuf::from("./.lash-data");
    let store_factory = Arc::new(SqliteSessionStoreFactory::new(data_dir.join("sessions")));
    let artifact_store = Arc::new(Store::open(&data_dir.join("artifacts.db")).await?);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits(model.clone(), Default::default(), 200_000, None)
                .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .build()?;
    // docs:end:sqlite-core
    Ok(())
}

async fn explicit_store(
    core: &LashCore,
    chat_id: &str,
    my_custom_persistence: impl lash::persistence::RuntimePersistence + 'static,
) -> anyhow::Result<()> {
    // docs:start:explicit-store
    let session = core
        .session(chat_id)
        .store(Arc::new(my_custom_persistence))
        .open()
        .await?;
    // docs:end:explicit-store
    Ok(())
}

async fn postgres_core(database_url: String) -> anyhow::Result<()> {
    // docs:start:postgres-core
    use std::sync::Arc;

    use lash_postgres_store::PostgresStorage;
    use lash_s3_store::S3AttachmentStore;

    let storage = PostgresStorage::connect(&database_url).await?;
    let attachments = S3AttachmentStore::builder("lash-attachments", "us-east-1")
        .endpoint_url("http://localhost:9000") // omit for AWS S3
        .access_key_id("minioadmin")
        .secret_access_key("minioadmin")
        .path_style(true)
        .prefix("prod/lash")
        .build()?;

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        Arc::new(storage.lashlang_artifact_store()),
    );
    let core = lash::LashCore::rlm_builder(factory)
        .store_factory(Arc::new(storage.session_store_factory()))
        .process_registry(Arc::new(storage.process_registry()))
        .trigger_store(Arc::new(storage.trigger_store()))
        .attachment_store(Arc::new(attachments))
        // provider, model, effect host...
        .build()?;
    // docs:end:postgres-core
    Ok(())
}

fn audit_process_cleanup(_report: lash::process::ProcessSessionDeleteReport) -> anyhow::Result<()> {
    Ok(())
}

async fn delete_session(core: &LashCore, chat_id: &str) -> anyhow::Result<()> {
    // docs:start:delete-session
    let effect_host = core.effect_host();
    let scope = effect_host.scoped(core.session_delete_scope(chat_id).await?)?;
    let report = core.delete_session(chat_id, scope).await?;

    if let Some(process_report) = report.process {
        audit_process_cleanup(process_report)?;
    }
    // docs:end:delete-session
    Ok(())
}

fn persist(_turn: TurnOutput) -> anyhow::Result<()> {
    Ok(())
}

fn retry_or_report(_err: lash::runtime::RuntimeError, _session: LashSession) -> anyhow::Result<()> {
    Ok(())
}

fn retry_later(_err: lash::runtime::RuntimeError) -> anyhow::Result<()> {
    Ok(())
}

async fn commit_conflict_retry(
    core: &LashCore,
    session: &LashSession,
    chat_id: &str,
    input: TurnInput,
) -> anyhow::Result<()> {
    // docs:start:commit-conflict-retry
    use lash::runtime::RuntimeErrorCode;

    match session.turn(input).run().await {
        Ok(turn) => persist(turn)?,
        Err(lash::EmbedError::Runtime(err))
            if err.code == RuntimeErrorCode::SessionExecutionBusy =>
        {
            retry_later(err)?;
        }
        Err(lash::EmbedError::Runtime(err))
            if err.code == RuntimeErrorCode::SessionExecutionLeaseLost =>
        {
            // The durable lane moved to another owner before commit: reopen and retry.
            let session = core.session(chat_id).open().await?;
            retry_or_report(err, session)?;
        }
        Err(lash::EmbedError::Runtime(err))
            if err.code == RuntimeErrorCode::StoreCommitContended =>
        {
            // The failed commit published nothing: retry the same operation unchanged.
            retry_later(err)?;
        }
        Err(lash::EmbedError::Runtime(err)) if err.code == RuntimeErrorCode::StoreCommitFailed => {
            // The CAS backstop fired: reload and retry.
            let session = core.session(chat_id).open().await?;
            retry_or_report(err, session)?;
        }
        Err(other) => bail!(other),
    }
    // docs:end:commit-conflict-retry
    Ok(())
}

async fn shared_factory(
    provider: ProviderHandle,
    model: String,
    model_variant: String,
    data_dir: PathBuf,
    chat_id: &str,
) -> anyhow::Result<()> {
    // docs:start:shared-factory
    // One factory at boot, shared across every chat.
    let store_factory = Arc::new(SqliteSessionStoreFactory::new(
        data_dir.join("lash-sessions"),
    ));
    let artifact_store =
        Arc::new(lash_sqlite_store::Store::open(&data_dir.join("lash-artifacts.db")).await?);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    );
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits(
                model.clone(),
                lash::provider::ReasoningSelection::Effort(model_variant.clone()),
                200_000,
                None,
            )
            .expect("valid model metadata")
            .with_capability(adaptive_reasoning_capability()),
        )
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .build()?;

    // Per request: open a session keyed by the app's chat id.
    let session = core.session(chat_id).open().await?;
    // docs:end:shared-factory
    Ok(())
}

fn retry_reenable_later(_conflict: lash::triggers::TriggerOperationError) -> anyhow::Result<()> {
    Ok(())
}

async fn trigger_reenable(
    store: &dyn lash::triggers::TriggerStore,
    owner_scope: lash::triggers::TriggerOwnerScope,
    actor: lash::process::ProcessOriginator,
    subscription_key: &str,
) -> anyhow::Result<()> {
    // docs:start:trigger-reenable
    use lash::triggers::{TriggerCommand, TriggerCommandOutcome, TriggerSubscriptionFilter};

    // Read the live revision through the command surface, not with SQL.
    let TriggerCommandOutcome::List { records } = store
        .execute_command(
            "reenable-read",
            TriggerCommand::List {
                owner_scope: owner_scope.clone(),
                filter: TriggerSubscriptionFilter {
                    subscription_key: Some(subscription_key.to_string()),
                    ..TriggerSubscriptionFilter::default()
                },
            },
        )
        .await??
    else {
        unreachable!("List returns list records")
    };
    let Some(record) = records.first() else {
        anyhow::bail!("no live subscription for `{subscription_key}`");
    };

    // Enable is fenced on the revision just read, and the operation id makes the
    // retry idempotent.
    let outcome = store
        .execute_command(
            &format!("reenable:{subscription_key}:{}", record.revision),
            TriggerCommand::Enable {
                owner_scope,
                actor,
                subscription_key: subscription_key.to_string(),
                expected_revision: record.revision,
            },
        )
        .await?;
    match outcome {
        Ok(TriggerCommandOutcome::Mutation { receipt }) => {
            assert!(receipt.enabled);
        }
        Ok(_) => unreachable!("Enable returns one mutation receipt"),
        // A concurrent writer moved the row first: re-read and retry. Never
        // patch the row by hand.
        Err(conflict) => retry_reenable_later(conflict)?,
    }
    // docs:end:trigger-reenable
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
        cache_control: None,
        stream_termination: None,
        sampling: lash::provider::SamplingCapability::Configurable,
    }
}
