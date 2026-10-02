//! Real Anthropic parsing through native Restate turns and SQL persistence.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use lash::StoreSet;
use lash_core::provider::{ProviderHandle, ProviderOptions, ProviderReliability};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use lash_provider_anthropic::AnthropicProvider;
use lash_restate_postgres_workers_e2e::local_restate::LocalRestate;

#[derive(Debug)]
struct FixtureTransport {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmHttpTransport for FixtureTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, lash_core::facade_support::LlmTransportError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let last_index = if call == 0 { 2 } else { 1 };
        let body = format!(
            concat!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":7}}}}}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"kept\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "data: {{\"type\":\"content_block_start\",\"index\":{last_index},\"content_block\":{{\"type\":\"text\"}}}}\n\n",
                "data: {{\"type\":\"content_block_delta\",\"index\":{last_index},\"delta\":{{\"type\":\"text_delta\",\"text\":\"answer\"}}}}\n\n",
                "data: {{\"type\":\"content_block_stop\",\"index\":{last_index}}}\n\n",
                "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\n",
                "data: {{\"type\":\"message_stop\"}}\n\n",
            ),
            last_index = last_index,
        );
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: LlmHttpBody::buffered(body),
        })
    }
}

async fn witness(stores: Arc<dyn StoreSet>, label: &str) -> Result<()> {
    let restate = LocalRestate::from_env()?;
    let identity = format!("bounds{}", uuid::Uuid::new_v4().simple());
    let engine = Arc::new(lash_restate::RestateEngine::new(
        stores,
        lash::restate::RestateConfig::new(
            restate.ingress_url.clone(),
            restate.admin_url.clone(),
            restate.authority.clone(),
        )
        .with_namespace(lash_restate::RestateNamespace::new(&identity)?),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = AnthropicProvider::new("fixture-key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..Default::default()
        })
        .with_transport(Arc::new(FixtureTransport {
            calls: Arc::clone(&calls),
        }));
    let core = lash::LashCore::standard_builder(lash::Backend::new(engine.clone()))
        .serve_test_model(
            ProviderHandle::new(provider.into_components()),
            lash::ModelMetadata::builder("fixture-model")
                .context_window_tokens(200_000)
                .max_output_tokens(4096)
                .build()?,
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            label, &identity,
        ))?;
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)?;
    let deployment = restate
        .serve(&engine, engine.endpoint_builder(worker)?.build())
        .await?;
    core.session(&identity)
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "fixture-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await?;
    let session = core.session(&identity).open().await?;
    let failed = session
        .send(lash::TurnInput::text("malformed fixture"))
        .id("malformed")
        .output()
        .await?;
    assert_eq!(
        failed.result.outcome,
        lash::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError),
        "{label}: sparse provider starts must settle as a failed turn",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "{label}: no retry");
    assert!(
        failed.result.errors.iter().any(|issue| {
            issue.kind == lash_core::TurnFailureKind::LlmProvider
                && issue.provider_failure_kind == Some(lash_core::ProviderFailureKind::Stream)
                && issue.retryable == Some(false)
        }),
        "{label}: classified terminal stream failure",
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    session.close().await?;
    let session = core.session(&identity).open().await?;
    let history_text: String = session
        .read_view()
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter().map(|part| part.content()))
        .collect();
    let reattached = session.attach_id("malformed").output().await?;
    assert_eq!(reattached.result.outcome, failed.result.outcome);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "{label}: attach does not retry"
    );
    assert!(
        !history_text.contains("answer"),
        "{label}: no rejected block"
    );
    let recovered = session
        .send(lash::TurnInput::text("valid multi-block fixture"))
        .id("valid")
        .output()
        .await?;
    assert!(matches!(
        recovered.result.outcome,
        lash::TurnOutcome::Finished(_)
    ));
    assert_eq!(recovered.assistant_message(), Some("kept\n\nanswer"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    session.close().await?;
    core.shutdown().await?;
    drop(deployment);
    println!(
        "{label}: malformed turn settled once, durable failure reattached, multi-block recovery completed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires native Restate from scripts/ci/with-service.sh restate"]
async fn native_restate_sqlite_memory_and_file_refuse_sparse_streams() -> Result<()> {
    witness(
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await?),
        "sqlite-memory",
    )
    .await?;
    let scratch = tempfile::tempdir()?;
    witness(
        Arc::new(lash_sqlite_store::SqliteStoreSet::open(scratch.path().join("sessions")).await?),
        "sqlite-file",
    )
    .await
}

#[tokio::test]
#[ignore = "requires native Restate and PostgreSQL from scripts/ci/with-service.sh"]
async fn native_restate_postgres_refuses_sparse_streams() -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").context("PostgreSQL service URL")?;
    let storage = lash_postgres_store::PostgresStorage::connect(&url).await?;
    let attachments = Arc::new(lash::persistence::FileAttachmentStore::new(scratch.path()));
    witness(
        Arc::new(lash_postgres_store::PostgresStoreSet::new(
            &storage,
            attachments,
        )),
        "postgres",
    )
    .await
}
