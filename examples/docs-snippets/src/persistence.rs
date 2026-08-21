//! Compiled sources for the Rust snippets on `docs/persistence.html`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use lash::provider::ProviderHandle;
use lash::{LashCore, LashSession, TurnInput, TurnOutput};
use lash_sqlite_store::SqliteSessionStoreFactory;

async fn sqlite_core(
    provider: ProviderHandle,
    model: String,
    data_dir: PathBuf,
) -> anyhow::Result<()> {
    // docs:start:sqlite-core
    use std::sync::Arc;

    use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

    let store_factory = Arc::new(SqliteSessionStoreFactory::new(data_dir.join("sessions")));
    let artifact_store = Arc::new(Store::open(&data_dir.join("artifacts.db")).await?);

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        artifact_store.clone(),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(
            lash::ModelSpec::builder(model.clone())
                .context_window_tokens(200_000)
                .build()
                .expect("valid model metadata"),
        )
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .process_env_store(artifact_store)
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;
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

async fn postgres_core(
    database_url: String,
    provider: ProviderHandle,
    model: lash::ModelSpec,
) -> anyhow::Result<()> {
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
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        Arc::new(storage.lashlang_artifact_store()),
    );
    let core = build_persistent_core(
        factory,
        provider,
        model,
        Arc::new(storage.session_store_factory()),
        Arc::new(storage.process_registry()),
        Arc::new(storage.trigger_store()),
        Arc::new(storage.effect_host()),
        Arc::new(attachments),
        Arc::new(storage.process_env_store()),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_persistent_core(
    factory: lash::rlm::RlmProtocolPluginFactory,
    provider: ProviderHandle,
    model: lash::ModelSpec,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    process_registry: Arc<dyn lash::process::ProcessRegistry>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    effect_host: Arc<dyn lash::durability::EffectHost>,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
) -> lash::Result<LashCore> {
    lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .process_registry(process_registry)
        .trigger_store(trigger_store)
        .effect_host(effect_host)
        .attachment_store(attachment_store)
        .process_env_store(process_env_store)
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())
}
// docs:end:postgres-core

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

fn requires_replay_mismatch_drain(err: &lash::runtime::RuntimeError) -> bool {
    // Store-qualified codes remain useful in logs, while host policy branches
    // on the typed semantic classification and survives backend renames.
    err.code.is_replay_mismatch()
}

#[test]
fn replay_mismatch_classification_is_host_usable() {
    let mut error = lash::runtime::RuntimeError::new(
        lash::runtime::RuntimeErrorCode::from_wire_code("sqlite_effect_replay_hash_conflict"),
        "recorded effect diverged",
    );
    error.summary = Some(lash::runtime::RuntimeEffectReplayMismatchReport {
        divergent_path_count: 1,
        first_divergent_paths: vec!["command.duration_ms".to_string()],
    });
    assert!(requires_replay_mismatch_drain(&error));
    let summary = error.summary.as_ref().expect("typed divergence summary");
    assert_eq!(summary.divergent_path_count, 1);
    assert_eq!(summary.first_divergent_paths, ["command.duration_ms"]);
}

#[test]
fn process_command_refusal_codes_are_stable_for_effect_hosts() {
    use lash::runtime::RuntimeErrorCode;

    let not_visible = RuntimeErrorCode::ProcessNotVisible;
    let already_terminal = RuntimeErrorCode::ProcessAlreadyTerminal;
    let no_longer_retained = RuntimeErrorCode::ProcessNoLongerRetained;

    assert_eq!(not_visible.as_str(), "process_not_visible");
    assert_eq!(already_terminal.as_str(), "process_already_terminal");
    assert_eq!(no_longer_retained.as_str(), "process_no_longer_retained");
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
            if err.code == RuntimeErrorCode::SessionExecutionLeaseLost =>
        {
            // The durable lane moved to another owner before commit: reopen and retry.
            let session = core.session(chat_id).open().await?;
            retry_or_report(err, session)?;
        }
        Err(lash::EmbedError::Runtime(err))
            if err.code == RuntimeErrorCode::SessionExecutionLaneBusy =>
        {
            // A durable workflow controller's queued drain found the lane held by
            // a live executor and stopped waiting: nothing was consumed, so let
            // the engine's retry policy re-drive this invocation unchanged.
            retry_later(err)?;
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
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        artifact_store.clone(),
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
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
            data_dir.join("attachments"),
        )))
        .process_env_store(artifact_store)
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

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

fn assert_facade_exports_typed_attachment_parse_errors() {
    use lash::attachments::{AttachmentId, InvalidAttachmentId, InvalidMediaType, MediaType};
    use std::error::Error;

    let invalid_attachment_id = match AttachmentId::parse("../escape") {
        Err(error) => error,
        Ok(_) => panic!("a path traversal id must be rejected"),
    };
    assert!(<InvalidAttachmentId as Error>::source(&invalid_attachment_id).is_none());

    let invalid_media_type = match MediaType::parse("image//png") {
        Err(error) => error,
        Ok(_) => panic!("a malformed MIME type must be rejected"),
    };
    assert!(<InvalidMediaType as Error>::source(&invalid_media_type).is_none());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_persistence_builders_resolve() {
        let data_dir = tempfile::tempdir().expect("temporary docs-snippet directory");
        sqlite_core(
            crate::test_support::provider(),
            "docs-snippet-test".to_string(),
            data_dir.path().to_path_buf(),
        )
        .await
        .expect("SQLite persistence snippet must build");

        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::new(
                lash::rlm::ExecutionBound::instructions(1_000_000),
                lash::rlm::ExecutionBound::secs(30),
                lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
            ),
            Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
        );
        build_persistent_core(
            factory,
            crate::test_support::provider(),
            crate::test_support::model(),
            Arc::new(lash::persistence::InMemorySessionStoreFactory::new()),
            Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::memory()
                    .await
                    .expect("in-memory process registry"),
            ),
            Arc::new(lash::triggers::InMemoryTriggerStore::default()),
            Arc::new(lash::durability::InlineEffectHost::default()),
            Arc::new(lash::persistence::InMemoryAttachmentStore::new()),
            Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::new()),
        )
        .expect("Postgres wiring snippet must build with test stores");

        shared_factory(
            crate::test_support::provider(),
            "docs-snippet-test".to_string(),
            "medium".to_string(),
            data_dir.path().to_path_buf(),
            "docs-snippet-session",
        )
        .await
        .expect("shared-factory snippet must build");

        assert_facade_exports_typed_attachment_parse_errors();
    }
}
